//! The inbox worker: every mail the webhook stored is fetched, forwarded,
//! read and filed here.
//!
//! The webhook (`inbound.rs`) stores an envelope and wakes this loop. For
//! each due mail the loop fetches the body from Resend, keeps the
//! attachments (a PDF's text pulled out for the model), forwards the whole
//! thing once to the address the account signed in with, asks the
//! tool-less extractor what it is, records the arrival where it belongs,
//! and says one line on Telegram. Every step is idempotent per row, so a
//! retry after a crash mid-way does the remaining work and repeats none
//! of the visible parts: the forward is recorded on the row, and the
//! attachments are fetched only while the row has none.

use crate::resend::{AttachmentMeta, Meta, Outgoing, ResendClient};
use scout_core::core::Core;
use scout_core::inbox::{MailToWork, Placement};
use std::sync::Arc;
use std::time::Duration;

/// How many characters of body-plus-attachments the model is handed. A
/// booking confirmation is a page or two; past this it is a newsletter.
pub const TEXT_CAP: usize = 24_000;
/// How much of the body is kept on the row: the spec's storage cap, wider
/// than `TEXT_CAP` because what the model does not need to read, a person
/// may. The page does not read it yet — showing the stored text is a later
/// slice — so today the row's copy serves the retries, which read and
/// forward from it rather than asking Resend again.
pub const BODY_CAP: usize = 512 * 1024;
/// The most attachment bytes one forward carries. Resend takes 40 MB a
/// message, base64 adds a third, and a forward the provider refuses for
/// its size is a forward nobody gets — better one without the last file.
pub const FORWARD_CAP: usize = 25 * 1024 * 1024;
/// The most bytes one attachment may be. Above this it is not stored and
/// not forwarded; the mail still is.
pub const ATTACHMENT_CAP: usize = 10 * 1024 * 1024;
/// The most attachments kept per mail.
pub const ATTACHMENTS_PER_MAIL: usize = 5;
/// How many mails one pass takes.
const BATCH: usize = 10;
/// The pass runs on a wake-up, and on this clock in case one was missed —
/// a mail stored just before a restart has no one to wake.
const TICK: Duration = Duration::from_secs(60);
/// After this many failed attempts the mail is given up on and shown as
/// Other mail. The store enforces the same number in its view; this one
/// decides when to say so.
const ATTEMPTS: i64 = scout_core::inbox::MAIL_ATTEMPTS;
/// A bound on one request to Resend or its storage. Long enough for a
/// ten-megabyte attachment on a slow link, short enough that a hung
/// download does not stall the whole inbox.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a PDF may take to give up its text before the worker moves on
/// without it.
const PDF_BUDGET: Duration = Duration::from_secs(30);

/// The HTTP client the worker's `ResendClient` rides on: a timeout and a
/// bounded redirect policy, because `download` follows a URL the
/// attachment list named and would otherwise follow it anywhere.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .expect("a TLS-capable HTTP client")
}

/// The loop: a pass on every wake-up and every `TICK`, for as long as the
/// process lives. `from` is the address forwards are sent from.
///
/// Each pass runs in a task of its own so that a panic inside it — a
/// library choking on one odd mail — is a logged error and not the quiet
/// end of the worker. The attempt was counted before the work began, so a
/// mail that panics the pass fails after `ATTEMPTS` tries rather than
/// looping forever.
pub async fn run(core: Arc<Core>, client: ResendClient, from: String) {
    loop {
        tokio::select! {
            _ = core.inbox_waiting() => {}
            _ = tokio::time::sleep(TICK) => {}
        }
        let (core, client, from) = (core.clone(), client.clone(), from.clone());
        let pass = tokio::spawn(async move { work_once(&core, &client, &from, BATCH).await });
        // The payload is not logged here: a panic message can quote the
        // text it was slicing, and the default hook has already written it
        // to stderr, where a mail body in the log is one line and not two.
        if pass.await.is_err() {
            tracing::error!("an inbox pass panicked");
        }
    }
}

/// One pass over due mail, and another while the last one was a full
/// batch: a burst arrives at once and should not drain one batch a tick.
/// Each step is idempotent per row: forwarding is recorded on the row so
/// a retry never forwards twice; attachments are fetched only when the
/// row has none yet; the model is asked only while no reading is on
/// record.
pub async fn work_once(core: &Core, client: &ResendClient, from: &str, limit: usize) {
    loop {
        let due = match scout_core::inbox::mail_to_work(core, limit).await {
            Ok(due) => due,
            Err(e) => {
                tracing::error!(error = %e, "could not read the inbox");
                return;
            }
        };
        // `limit > 0` because an empty batch of a zero limit is "full".
        let full = limit > 0 && due.len() == limit;
        for m in due {
            if let Err(e) = work_one(core, client, from, &m).await {
                // The store is not answering. It would hand the same
                // batch back, so the pass ends here and the tick returns.
                tracing::warn!(error = %e, id = m.id, "inbox bookkeeping");
                return;
            }
        }
        if !full {
            return;
        }
    }
}

/// Why a pass could not finish a mail. The distinction decides whether the
/// attempt was spent: an outage says nothing about the mail.
enum Failure {
    /// Resend could not be reached, or said try later. The attempt is
    /// handed back; the spacing still applies.
    Transport(anyhow::Error),
    /// The mail itself could not be read — the model failed, the store
    /// refused, the provider answered no. The attempt stands.
    Reading(anyhow::Error),
}

/// A Resend error sorted into the two.
fn resend(e: anyhow::Error) -> Failure {
    if crate::resend::is_transient(&e) {
        Failure::Transport(e)
    } else {
        Failure::Reading(e)
    }
}

/// One mail. `Err` is the store failing at the bookkeeping around the
/// work; everything about the mail itself is handled here.
async fn work_one(core: &Core, client: &ResendClient, from: &str, m: &MailToWork) -> anyhow::Result<()> {
    // Counted before the work, so a crash mid-way still spends one.
    scout_core::inbox::mail_attempted(core, m.id).await?;
    // `process` settles the mail itself once the arrival is on record;
    // an `Ok` here means it did.
    let e = match process(core, client, from, m).await {
        Ok(()) => return Ok(()),
        Err(Failure::Transport(e)) => {
            tracing::warn!(error = %e, id = m.id, "resend was not there; the attempt is handed back");
            return scout_core::inbox::mail_unattempted(core, m.id).await;
        }
        Err(Failure::Reading(e)) => e,
    };
    let attempts = m.attempts + 1;
    tracing::warn!(error = %e, id = m.id, attempts, "an email could not be read");
    if attempts < ATTEMPTS {
        return Ok(());
    }
    scout_core::inbox::mail_failed(core, m.id, &format!("{e:#}")).await?;
    let text = "An email arrived that Scout could not read. It is under Other mail on goodscout.fyi/chat.";
    if let Err(e) = scout_core::inbox::nudge(core, m.account_id, m.id, text).await {
        tracing::warn!(error = %e, id = m.id, "could not nudge about a failed email");
    }
    Ok(())
}

/// What became of the forward, this pass or an earlier one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Forwarded {
    /// Sent — now, or on a pass before this one.
    Yes,
    /// The account has no email identity: nowhere to send it. The mail is
    /// settled as done on this pass; only a retry of a failed reading asks
    /// again, in case one was linked since.
    NoAddress,
    /// Resend refused it. Asking again would get the same answer.
    Refused,
    /// Resend was not there. Worth asking again.
    Later,
}

/// Everything one mail needs, in the order that makes a retry safe: body,
/// attachments, forward, then the model. The forward comes before the
/// model on purpose — the person gets their mail even when the model is
/// down. A forward Resend refuses is a warning, not a reason to leave the
/// mail unread; a forward Resend was not there for is owed, and `settle`
/// leaves the mail due so a later pass sends it — that pass finds the
/// reading on record and does nothing else.
async fn process(core: &Core, client: &ResendClient, from: &str, m: &MailToWork) -> Result<(), Failure> {
    // 0. A mail read on an earlier pass owes only its forward.
    if scout_core::inbox::mail_has_arrival(core, m.id).await.map_err(Failure::Reading)? {
        let forwarded = forward(core, client, from, m, &m.from, m.subject.clone(), m.text.clone(), m.html.clone()).await?;
        return settle(core, m, forwarded).await;
    }

    // 1. The body: the row's, when an earlier pass stored it, so a retry
    // does not depend on Resend; else the API's, since the webhook
    // carries none. The record's sender is preferred over the row's copy:
    // the same mail, seen in full rather than capped on the way in.
    //
    // The record's own part list comes out with the body, because it is
    // the only thing that says which parts are the mail's furniture — see
    // `winnow`. On the retry path the body is the row's and there is no
    // record to have; then the list is empty and only the no-name rule
    // applies, which is the same answer as a record that says nothing.
    let (sender, subject, text, html, listed, parts) = if m.text.is_some() || m.html.is_some() {
        (m.from.clone(), m.subject.clone(), m.text.clone(), m.html.clone(), true, Vec::new())
    } else {
        let r = client.received(&m.provider_id).await.map_err(resend)?;
        scout_core::inbox::mail_body(core, m.id, r.text.clone(), r.html.clone(), BODY_CAP).await.map_err(Failure::Reading)?;
        let sender = if r.from.is_empty() { m.from.clone() } else { r.from };
        (sender, r.subject.or_else(|| m.subject.clone()), r.text, r.html, !r.attachments.is_empty(), r.attachments)
    };

    // 2. Attachments, once. A download that fails or is over the cap
    // leaves a row with a name and no bytes, so the page can still say
    // the mail had one. The record already names the attachments; the
    // list call is the one that adds URLs, and most mail has nothing to
    // list — but a body from the row came without the record, so then
    // the list is asked.
    let stored = scout_core::inbox::attachment_texts(core, m.id).await.map_err(Failure::Reading)?;
    let texts = if stored.is_empty() && listed {
        let mut texts = Vec::new();
        let (keep, skipped) = winnow(&parts, client.attachments(&m.provider_id).await.map_err(resend)?);
        if skipped.any() {
            // Once per mail, counts only. Worth saying because a mail that
            // loses its ticket this way looks, from the trip card, exactly
            // like a mail that never had one.
            tracing::debug!(
                id = m.id,
                decoration = skipped.decoration,
                nameless = skipped.nameless,
                "parts left out of a mail: the body's own images, and parts with no filename"
            );
        }
        for meta in keep.into_iter().take(ATTACHMENTS_PER_MAIL) {
            let bytes = match client.download(&meta.download_url, ATTACHMENT_CAP).await {
                Ok(bytes) => Some(bytes),
                Err(e) => {
                    tracing::warn!(error = %e, id = m.id, "an attachment could not be fetched");
                    None
                }
            };
            let (bytes, text) = match bytes {
                Some(b) if meta.content_type == "application/pdf" => {
                    let (b, text) = pdf_text(m.id, b).await;
                    (Some(b), text)
                }
                other => (other, None),
            };
            scout_core::inbox::store_attachment(core, m.id, &meta.filename, &meta.content_type, bytes, text.clone())
                .await
                .map_err(Failure::Reading)?;
            texts.push((meta.filename, text));
        }
        texts
    } else {
        stored
    };

    // 3. Forward, once.
    let forwarded = forward(core, client, from, m, &sender, subject.clone(), text.clone(), html.clone()).await?;

    // 4. Extract, place, record, and only then settle.
    let parts: Vec<(&str, Option<&str>)> = texts.iter().map(|(f, t)| (f.as_str(), t.as_deref())).collect();
    let text = assemble(text.as_deref(), html.as_deref(), &parts, TEXT_CAP);
    let readings = scout_core::inbox::extract(core, &text).await.map_err(Failure::Reading)?;
    let bookings = readings.iter().filter(|e| e.booking).count();
    let (_, placement) =
        scout_core::inbox::record_arrivals(core, m.account_id, m.id, readings).await.map_err(Failure::Reading)?;
    settle(core, m, forwarded).await?;

    // 5. The nudge. From here nothing fails the mail: it is filed, and a
    // line that could not be queued is not a reason to pay the model to
    // read it again.
    let text = match placement {
        Some(p) => {
            let name = match scout_core::inbox::trip_name(core, m.account_id, p.id()).await {
                Ok(name) => name,
                Err(e) => {
                    tracing::warn!(error = %e, id = m.id, "could not name the trip for the nudge");
                    None
                }
            };
            arrival_nudge(p, name.as_deref().unwrap_or("a trip"), bookings)
        }
        // Not a booking, but from a place bookings come from: worth a line,
        // since the person may be waiting on it. Anything else is just
        // mail, and mail arrives quietly.
        None if looks_like_a_booking_site(&sender) => other_mail_nudge(&sender, subject.as_deref().unwrap_or_default(), forwarded),
        None => return Ok(()),
    };
    if let Err(e) = scout_core::inbox::nudge(core, m.account_id, m.id, &text).await {
        tracing::warn!(error = %e, id = m.id, "could not nudge about an arrival");
    }
    Ok(())
}

/// The forward, unless the row says it went. An account with no email
/// identity has nowhere to forward to; the mail is still read and filed.
/// A refusal is logged and the mail goes on; an outage is `Later`, for
/// `settle` to decide. Neither is an `Err`: the reading does not wait on
/// the forward. The store failing is.
#[allow(clippy::too_many_arguments)]
async fn forward(
    core: &Core,
    client: &ResendClient,
    from: &str,
    m: &MailToWork,
    sender: &str,
    subject: Option<String>,
    text: Option<String>,
    html: Option<String>,
) -> Result<Forwarded, Failure> {
    if m.forwarded {
        return Ok(Forwarded::Yes);
    }
    let Some(to) = scout_core::inbox::email_of(core, m.account_id).await.map_err(Failure::Reading)? else {
        return Ok(Forwarded::NoAddress);
    };
    let all = scout_core::inbox::attachment_bytes(core, m.id).await.map_err(Failure::Reading)?;
    let attachments = within_cap(all, FORWARD_CAP);
    let mail = Outgoing {
        from: from.into(),
        to,
        reply_to: Some(sender.to_string()),
        subject: subject.unwrap_or_default(),
        text,
        html,
        attachments,
    };
    match client.send(&mail).await {
        Ok(()) => {
            scout_core::inbox::mail_forwarded(core, m.id).await.map_err(Failure::Reading)?;
            Ok(Forwarded::Yes)
        }
        Err(e) if crate::resend::is_transient(&e) => {
            tracing::warn!(error = %e, id = m.id, "the forward did not go; reading the mail anyway, sending later");
            Ok(Forwarded::Later)
        }
        Err(e) => {
            tracing::warn!(error = %e, id = m.id, "the forward was refused; reading the mail anyway");
            Ok(Forwarded::Refused)
        }
    }
}

/// What one mail's listing lost, by reason. Counts only: a filename is
/// the sender's text and the log is not the place for it.
#[derive(Debug, Default, PartialEq, Eq)]
struct Skipped {
    /// Parts the received record marks as the mail's own furniture.
    decoration: usize,
    /// Parts with no filename, whatever the record says about them.
    nameless: usize,
}

impl Skipped {
    fn any(&self) -> bool {
        self.decoration + self.nameless > 0
    }
}

/// The listed parts worth keeping, and a count of what was left behind.
///
/// Two rules, and the order matters only to the counting:
///
/// A part the `record` calls `inline`, or gives a Content-ID, is how an
/// HTML mail carries the pictures it draws — a signature logo, a header
/// banner, a tracking pixel. It is not a file the traveller attached
/// anything to, and stored it becomes a row on the trip card competing
/// with the ticket. Neither field is documented on `GET
/// /emails/receiving/{id}`, so when the record carries neither this set
/// is empty and every part survives: the behaviour before this rule
/// existed, which is the safe way to be wrong about a provider.
///
/// A part with no filename goes regardless of what the record says. The
/// page renders it as "attachment 12" — its provider id — which tells the
/// reader nothing they can act on, and a real attachment has a name.
///
/// Winnowing happens before `ATTACHMENTS_PER_MAIL`, which is the point:
/// the cap must be spent on files, not on four logos and a ticket that
/// did not fit.
fn winnow(record: &[Meta], listed: Vec<AttachmentMeta>) -> (Vec<AttachmentMeta>, Skipped) {
    let decoration: std::collections::HashSet<&str> = record
        .iter()
        .filter(|m| {
            m.content_disposition.as_deref().is_some_and(|d| d.trim().eq_ignore_ascii_case("inline"))
                || m.content_id.as_deref().is_some_and(|c| !c.trim().is_empty())
        })
        .map(|m| m.id.as_str())
        .collect();
    let mut skipped = Skipped::default();
    let kept = listed
        .into_iter()
        .filter(|a| {
            if decoration.contains(a.id.as_str()) {
                skipped.decoration += 1;
                return false;
            }
            if a.filename.trim().is_empty() {
                skipped.nameless += 1;
                return false;
            }
            true
        })
        .collect();
    (kept, skipped)
}

/// The attachments that fit under `cap` bytes together, in order, the
/// rest dropped with a line saying how many — not which: a filename is
/// the sender's text.
fn within_cap(attachments: Vec<(String, Vec<u8>)>, cap: usize) -> Vec<(String, Vec<u8>)> {
    let mut total = 0usize;
    let (kept, dropped): (Vec<_>, Vec<_>) = attachments.into_iter().partition(|(_, bytes)| {
        let fits = total + bytes.len() <= cap;
        if fits {
            total += bytes.len();
        }
        fits
    });
    if !dropped.is_empty() {
        tracing::warn!(dropped = dropped.len(), "attachments left off a forward to stay under the size cap");
    }
    kept
}

/// The mail's end state once its reading is on record. Done, unless the
/// forward is owed and there are attempts left: then the row stays due,
/// its attempt spent, so a later pass sends the forward alone and the
/// count bounds how long that goes on. On the last attempt it is done
/// regardless, and the row says "not forwarded", which is the truth.
async fn settle(core: &Core, m: &MailToWork, forwarded: Forwarded) -> Result<(), Failure> {
    // `work_one` counted this pass before the work.
    let attempts = m.attempts + 1;
    if forwarded == Forwarded::Later && attempts < ATTEMPTS {
        tracing::info!(id = m.id, attempts, "read and filed; the forward waits for the next pass");
        return Ok(());
    }
    scout_core::inbox::mail_done(core, m.id).await.map_err(Failure::Reading)
}

/// The line for a mail from a booking site that was not a booking: where
/// it came from, what it said it was, and whether it reached the person
/// — said only when known. The subject is the sender's text, cut so a
/// stranger cannot fill the phone with it.
fn other_mail_nudge(sender: &str, subject: &str, forwarded: Forwarded) -> String {
    let subject: String = subject.chars().take(200).collect();
    let mut line = format!("Mail from {}: \"{subject}\".", domain_of(sender));
    match forwarded {
        Forwarded::Yes => line.push_str(" Forwarded to you."),
        Forwarded::NoAddress => line.push_str(" Not forwarded: no email on your account."),
        Forwarded::Refused | Forwarded::Later => {}
    }
    line
}

/// The line on the phone for the bookings that were placed: on a trip that
/// was already there, or on a draft made for them. `count` is how many the
/// one mail confirmed — a return ticket is two — and they share a trip, so
/// they share a line.
fn arrival_nudge(placement: Placement, name: &str, count: usize) -> String {
    let (what, review) = if count > 1 {
        (format!("{count} bookings arrived"), "Review them")
    } else {
        ("A booking arrived".to_string(), "Review it")
    };
    match placement {
        Placement::Trip(_) => format!("{what} for {name}. {review} on goodscout.fyi/chat."),
        Placement::Draft(_) => format!("{what} and started a draft trip, {name}. {review} on goodscout.fyi/chat."),
    }
}

/// A PDF's text, or `None` when there is none to be had, with the buffer
/// handed back so the caller does not clone ten megabytes to keep it.
///
/// Off the async threads: extraction is CPU-bound and a big scan takes
/// seconds. A panic counts as no text — pdf-extract is known to panic on
/// odd files, and an odd attachment must not take the worker down. Past
/// `PDF_BUDGET` the worker moves on without the text; the blocking thread
/// finishes on its own and its answer is dropped. The buffer is shared
/// with that thread, so only that late case pays for a copy.
async fn pdf_text(mail_id: i64, bytes: Vec<u8>) -> (Vec<u8>, Option<String>) {
    let shared = Arc::new(bytes);
    let theirs = shared.clone();
    let task = tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pdf_extract::extract_text_from_mem(&theirs)))
    });
    // The errors name the file's structure, never its content, so they
    // can be logged.
    let text = match tokio::time::timeout(PDF_BUDGET, task).await {
        Ok(Ok(Ok(Ok(text)))) => Some(text.trim().to_string()).filter(|t| !t.is_empty()),
        Ok(Ok(Ok(Err(e)))) => {
            tracing::warn!(error = %e, id = mail_id, "a PDF gave no text");
            None
        }
        Ok(Ok(Err(_))) => {
            tracing::warn!(id = mail_id, "pdf-extract panicked on an attachment");
            None
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, id = mail_id, "the PDF task did not finish");
            None
        }
        Err(_) => {
            tracing::warn!(id = mail_id, "a PDF took longer than the budget; going on without its text");
            None
        }
    };
    let bytes = Arc::try_unwrap(shared).unwrap_or_else(|still_shared| (*still_shared).clone());
    (bytes, text)
}

/// Body first, then each attachment's text under a header; HTML stripped to
/// text when there is no plain part; cut at `cap` characters.
pub fn assemble(text: Option<&str>, html: Option<&str>, attachments: &[(&str, Option<&str>)], cap: usize) -> String {
    let mut out = match (text, html) {
        (Some(t), _) if !t.trim().is_empty() => t.trim().to_string(),
        (_, Some(h)) => html_to_text(h),
        _ => String::new(),
    };
    for (name, text) in attachments {
        if let Some(t) = text.filter(|t| !t.trim().is_empty()) {
            out.push_str(&format!("\n\n--- attachment: {name} ---\n{}", t.trim()));
        }
    }
    out.chars().take(cap).collect()
}

/// Enough of an HTML-to-text pass for a confirmation email: `<style>` and
/// `<script>` blocks dropped, tags replaced by spaces, the six common
/// entities decoded, whitespace collapsed. Not a sanitiser — the result
/// goes to the model as text and to the DOM only through `textContent`.
///
/// One pass over the bytes, matching tag names ASCII-case-insensitively
/// in place. Every index it holds is a char boundary: a tag's bytes are
/// skipped up to an ASCII `>`, a block's up to an ASCII `</…>`, and text
/// is taken a whole char at a time — so a mail in any script is safe, and
/// the cost is linear in the mail rather than a copy per block found.
pub fn html_to_text(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut text = String::with_capacity(html.len());
    let mut i = 0;
    let mut in_tag = false;
    while i < bytes.len() {
        if in_tag {
            in_tag = bytes[i] != b'>';
            i += 1;
        } else if bytes[i] == b'<' {
            text.push(' ');
            match [&b"style"[..], b"script"].into_iter().find(|tag| opens_block(bytes, i + 1, tag)) {
                Some(tag) => {
                    let close = [b"</", tag, b">"].concat();
                    i = find_ci(bytes, i + 1, &close).map(|at| at + close.len()).unwrap_or(bytes.len());
                }
                None => {
                    in_tag = true;
                    i += 1;
                }
            }
        } else {
            let c = html[i..].chars().next().expect("i is on a char boundary");
            text.push(c);
            i += c.len_utf8();
        }
    }
    let text = text
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        // Last, so `&amp;lt;` comes out as the literal `&lt;` it encodes.
        .replace("&amp;", "&");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether `bytes[at..]` opens a `<tag …>` block: the name, then
/// whitespace, `>`, `/` or the end of the input — so `<styles>` is not
/// `<style>`.
fn opens_block(bytes: &[u8], at: usize, tag: &[u8]) -> bool {
    let rest = &bytes[at.min(bytes.len())..];
    starts_with_ci(rest, tag)
        && matches!(rest.get(tag.len()), None | Some(b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/'))
}

fn starts_with_ci(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() >= needle.len() && hay.iter().zip(needle).all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// The first ASCII-case-insensitive `needle` at or after `from`.
fn find_ci(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&at| starts_with_ci(&hay[at..], needle))
}

/// Senders whose mail is worth a line on the phone even when the model saw
/// no booking in it: a "your flight has changed" from one of these is
/// something the person is waiting on. Registrable domains, matched whole
/// or as a parent — `mail.ryanair.com` is Ryanair, `ryanair.example` is not.
const BOOKING_DOMAINS: &[&str] = &[
    "booking.com", "expedia.com", "airbnb.com", "getyourguide.com", "tiqets.com", "trainline.com", "ryanair.com",
    "klm.com", "flytap.com", "easyjet.com", "transavia.com", "lufthansa.com", "vueling.com", "eurostar.com",
    "ns.nl", "hotels.com", "agoda.com", "hostelworld.com", "viator.com", "civitatis.com",
];

fn looks_like_a_booking_site(from: &str) -> bool {
    let d = domain_of(from);
    BOOKING_DOMAINS.iter().any(|b| d == *b || d.strip_suffix(b).is_some_and(|head| head.ends_with('.')))
}

/// The part after the `@`, lowercased, out of either `a@b.c` or
/// `Name <a@b.c>`; the whole string when there is no `@`.
fn domain_of(from: &str) -> String {
    let address = from.rsplit_once('<').map(|(_, rest)| rest).unwrap_or(from);
    let address = address.trim().trim_end_matches('>');
    address.rsplit_once('@').map(|(_, d)| d).unwrap_or(address).trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resend::tests::resend_like;
    use crate::tests::{admitted, open_round, test_app, test_app_with_model};
    use scout_core::inbox::MailIn;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A part as the received record lists it.
    fn part(id: &str, filename: Option<&str>, disposition: Option<&str>, cid: Option<&str>) -> Meta {
        Meta {
            id: id.into(),
            filename: filename.map(Into::into),
            content_type: Some("application/octet-stream".into()),
            size: Some(1),
            content_disposition: disposition.map(Into::into),
            content_id: cid.map(Into::into),
        }
    }

    /// The same part as the list endpoint returns it, with a URL.
    fn listed(id: &str, filename: &str) -> AttachmentMeta {
        AttachmentMeta {
            id: id.into(),
            filename: filename.into(),
            size: 1,
            content_type: "application/octet-stream".into(),
            download_url: format!("https://example.invalid/dl/{id}"),
        }
    }

    #[test]
    fn the_mails_decoration_is_left_off_and_a_record_that_says_nothing_leaves_everything_on() {
        // The four shapes one leg actually arrived with: a logo the body
        // draws, a calendar invite the mailer marked inline, a part sent
        // with no filename at all, and the one thing the traveller wants.
        let record = [
            part("att_logo", Some("logo.png"), Some("inline"), Some("<logo@mailer>")),
            part("att_cal", Some("Add_to_your_calendar.ics"), Some("INLINE"), None),
            part("att_12", None, None, None),
            part("att_tkt", Some("Electronic_ticket.pdf"), Some("attachment"), None),
        ];
        let all = vec![
            listed("att_logo", "logo.png"),
            listed("att_cal", "Add_to_your_calendar.ics"),
            // The list endpoint defaults a missing name to the empty
            // string, which is what `attachmentLinks` renders as
            // "attachment 12" — a label that tells the reader nothing.
            listed("att_12", ""),
            listed("att_tkt", "Electronic_ticket.pdf"),
        ];
        let (kept, skipped) = winnow(&record, all.clone());
        assert_eq!(kept.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["att_tkt"]);
        assert_eq!(skipped, Skipped { decoration: 2, nameless: 1 });

        // A record that carries neither field — which is every record
        // Resend is documented to send — changes nothing. Only the part
        // with no name goes, because a real attachment has one.
        let silent: Vec<Meta> = record.iter().map(|m| part(&m.id, m.filename.as_deref(), None, None)).collect();
        let (kept, skipped) = winnow(&silent, all.clone());
        assert_eq!(kept.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["att_logo", "att_cal", "att_tkt"]);
        assert_eq!(skipped, Skipped { decoration: 0, nameless: 1 });

        // And no record at all — the retry path, where the body came off
        // the row and Resend was never asked — is the same again.
        let (kept, _) = winnow(&[], all);
        assert_eq!(kept.len(), 3);
    }

    #[test]
    fn a_name_that_is_only_spaces_is_no_name() {
        // `attachmentLinks` would render this as a chip the reader cannot
        // see the label of, which is worse than not offering it.
        let (kept, skipped) = winnow(&[], vec![listed("att_1", "   ")]);
        assert!(kept.is_empty());
        assert_eq!(skipped, Skipped { decoration: 0, nameless: 1 });
    }

    #[test]
    fn the_extractor_sees_the_body_then_each_attachments_text_within_the_cap() {
        let text = assemble(Some("Body"), None, &[("ticket.pdf", Some("Row 12")), ("logo.png", None)], 50);
        assert_eq!(text, "Body\n\n--- attachment: ticket.pdf ---\nRow 12");
        let long = assemble(Some(&"x".repeat(100)), None, &[], 20);
        assert_eq!(long.chars().count(), 20);
        let html_only = assemble(None, Some("<p>Hi <b>there</b></p>"), &[], 50);
        assert_eq!(html_only, "Hi there");
    }

    #[test]
    fn html_loses_its_styles_and_tags_and_gets_its_entities_back() {
        let html = "<html><head><STYLE>p { color: red }</STYLE></head><body><p>Tom&nbsp;&amp;&nbsp;Jerry</p><script>alert(1)</script><p>&lt;3&gt; &quot;ok&quot; it&#39;s</p></body></html>";
        assert_eq!(html_to_text(html), "Tom & Jerry <3> \"ok\" it's");
        // An unclosed style block is dropped to the end rather than leaking its CSS.
        assert_eq!(html_to_text("a<style>b{c}"), "a");
        // Only the tag itself: a longer name that starts the same is a tag like any other.
        assert_eq!(html_to_text("<styles>x</styles>"), "x");
        assert_eq!(html_to_text("<style type=\"text/css\">x</style>y<script\n>z</SCRIPT>w"), "y w");
    }

    #[test]
    fn html_in_any_script_is_stripped_without_panicking_or_looping() {
        // Multi-byte chars whose lowercase has a different byte length
        // would put an offset taken from the lowercased copy mid-char.
        assert_eq!(html_to_text("İ<style>İ</style>é"), "İ é");
        // And an unclosed block after such chars once found itself forever.
        assert_eq!(html_to_text("İİİİİİ<style"), "İİİİİİ");
        assert_eq!(html_to_text("Café — naïve 日本語 🙂"), "Café — naïve 日本語 🙂");
        assert_eq!(html_to_text("<a title=\"é\">ü</a>"), "ü");
    }

    #[tokio::test]
    async fn garbage_is_not_a_pdf_and_says_so_without_panicking() {
        let (bytes, text) = pdf_text(1, b"not a pdf at all".to_vec()).await;
        assert_eq!((bytes.as_slice(), text), (&b"not a pdf at all"[..], None));
        assert_eq!(pdf_text(1, Vec::new()).await, (Vec::new(), None));
    }

    #[test]
    fn a_booking_site_is_known_by_the_registrable_domain_of_the_sender() {
        assert!(looks_like_a_booking_site("noreply@booking.com"));
        assert!(looks_like_a_booking_site("Ryanair <no-reply@mail.ryanair.com>"));
        assert!(looks_like_a_booking_site("info@NS.nl"));
        assert!(!looks_like_a_booking_site("friend@example.com"));
        assert!(!looks_like_a_booking_site("noreply@plans.nl"), "a suffix is not a parent domain");
        assert!(!looks_like_a_booking_site("noreply@expedia.evil.example"), "a prefix is not the site");
        assert_eq!(domain_of("Hotel <hotel@Example.com>"), "example.com");
    }

    #[test]
    fn the_other_mail_nudge_says_only_what_it_knows_about_the_forward() {
        assert_eq!(other_mail_nudge("noreply@booking.com", "Changed", Forwarded::Yes), "Mail from booking.com: \"Changed\". Forwarded to you.");
        assert_eq!(
            other_mail_nudge("noreply@booking.com", "Changed", Forwarded::NoAddress),
            "Mail from booking.com: \"Changed\". Not forwarded: no email on your account."
        );
        assert_eq!(other_mail_nudge("noreply@booking.com", "Changed", Forwarded::Later), "Mail from booking.com: \"Changed\".");
        assert_eq!(other_mail_nudge("noreply@booking.com", "Changed", Forwarded::Refused), "Mail from booking.com: \"Changed\".");
        let long = other_mail_nudge("noreply@booking.com", &"é".repeat(300), Forwarded::Yes);
        assert_eq!(long.chars().count(), "Mail from booking.com: \"\". Forwarded to you.".len() + 200);
    }

    #[test]
    fn a_forward_carries_what_fits_under_the_cap_in_order() {
        let atts = vec![("a".to_string(), vec![0; 6]), ("b".to_string(), vec![0; 6]), ("c".to_string(), vec![0; 2])];
        let kept: Vec<String> = within_cap(atts, 10).into_iter().map(|(n, _)| n).collect();
        assert_eq!(kept, ["a", "c"], "b did not fit; c, after it, did");
    }

    #[test]
    fn the_nudge_says_whether_the_booking_joined_a_trip_or_started_one() {
        assert_eq!(arrival_nudge(Placement::Trip(1), "Lisbon", 1), "A booking arrived for Lisbon. Review it on goodscout.fyi/chat.");
        assert_eq!(
            arrival_nudge(Placement::Draft(1), "Lisbon, October", 1),
            "A booking arrived and started a draft trip, Lisbon, October. Review it on goodscout.fyi/chat."
        );
        // A round trip is two bookings off one mail, and the line counts
        // them rather than saying "a booking" twice or once.
        assert_eq!(arrival_nudge(Placement::Trip(1), "Lisbon", 2), "2 bookings arrived for Lisbon. Review them on goodscout.fyi/chat.");
        assert_eq!(
            arrival_nudge(Placement::Draft(1), "Hong Kong, November", 2),
            "2 bookings arrived and started a draft trip, Hong Kong, November. Review them on goodscout.fyi/chat."
        );
    }

    /// A model that answers every completion with `answer`, on the same
    /// server as the Resend stand-in: rig appends `/chat/completions` to
    /// the base URL, so the paths never meet.
    async fn model_saying(server: &MockServer, answer: &str) {
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "w1", "model": "m",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": answer}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            })))
            .mount(server).await;
    }

    const FROM: &str = "scout@send.goodscout.fyi";

    fn a_mail(provider_id: &str) -> MailIn {
        MailIn { provider_id: provider_id.into(), from: "hotel@example.com".into(), subject: Some("Your booking".into()), text: None, html: None, truncated: false }
    }

    fn forwards(reqs: &[wiremock::Request]) -> usize {
        reqs.iter().filter(|r| r.url.path() == "/emails").count()
    }

    #[tokio::test]
    async fn one_pass_forwards_fetches_and_records_the_outcome() {
        let server = MockServer::start().await;
        resend_like(&server).await;
        // `test_app` points the model at a closed port, so extraction
        // fails plainly and the rest of the pipeline is what is under test.
        let (_app, core, _dir) = test_app().await;
        open_round(&core, "autumn", 5).await;
        let a = admitted(&core, "111").await;
        scout_core::inbox::seed_email_identity_for_tests(&core, a, "me@example.com").await.unwrap();
        let mail_id = scout_core::inbox::record_mail(&core, a, a_mail("re_1")).await.unwrap().unwrap();
        let client = ResendClient::new(reqwest::Client::new(), "k".into(), server.uri());
        work_once(&core, &client, FROM, 10).await;
        // Forwarded (one POST /emails), attachment stored, extraction failed on the closed port → attempts 1, status extracting.
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(forwards(&reqs), 1);
        let forward: serde_json::Value = serde_json::from_slice(&reqs.iter().find(|r| r.url.path() == "/emails").unwrap().body).unwrap();
        assert_eq!(forward["to"], json!(["me@example.com"]));
        assert_eq!(forward["reply_to"], json!(["hotel@example.com"]));
        assert_eq!(forward["attachments"][0]["filename"], "ticket.pdf");
        let stored = scout_core::inbox::attachment_texts(&core, mail_id).await.unwrap();
        assert_eq!(stored, vec![("ticket.pdf".to_string(), None)]);
        let view = scout_core::inbox::view(&core, a, "goodscout.fyi").await.unwrap();
        assert!(view.pending.is_empty() && view.other.is_empty(), "not decided yet");
        // Attempted a moment ago: the next pass leaves it alone until the spacing has passed.
        work_once(&core, &client, FROM, 10).await;
        assert_eq!(server.received_requests().await.unwrap().iter().filter(|r| r.url.path() == "/emails/receiving/re_1").count(), 1, "a fresh attempt is not retried at once");
        scout_core::inbox::age_attempts_for_tests(&core, mail_id).await.unwrap();
        work_once(&core, &client, FROM, 10).await;
        scout_core::inbox::age_attempts_for_tests(&core, mail_id).await.unwrap();
        work_once(&core, &client, FROM, 10).await;
        let view = scout_core::inbox::view(&core, a, "goodscout.fyi").await.unwrap();
        assert_eq!(view.other[0].reason, "failed", "three attempts, then Other mail");
        assert!(view.other[0].forwarded);
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(forwards(&reqs), 1, "forwarded once, not per attempt");
        assert_eq!(reqs.iter().filter(|r| r.url.path() == "/dl/att_1").count(), 1, "attachments fetched once, not per attempt");
        let fetches = |reqs: &[wiremock::Request]| reqs.iter().filter(|r| r.url.path() == "/emails/receiving/re_1").count();
        assert_eq!(fetches(&reqs), 1, "the body fetched once: the row's copy serves the retries");
        // A fourth pass finds nothing due.
        scout_core::inbox::age_attempts_for_tests(&core, mail_id).await.unwrap();
        work_once(&core, &client, FROM, 10).await;
        assert_eq!(fetches(&server.received_requests().await.unwrap()), 1);
    }

    #[tokio::test]
    async fn only_the_ticket_is_kept_out_of_a_mail_of_logos() {
        // The leg the owner reported: four parts listed, one of them the
        // thing they came for. Every step is the real one — record, list,
        // download, store — so this fails if the winnowing is done
        // anywhere the worker does not actually look.
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/emails/receiving/re_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "re_1", "from": "airline@example.com", "subject": "Your booking",
                "text": "Row 12", "html": "<p>Row 12</p>",
                "attachments": [
                    {"id": "att_logo", "filename": "attachment-1.png", "content_type": "image/png", "size": 9, "content_disposition": "inline"},
                    {"id": "att_cal", "filename": "Add_to_your_calendar.ics", "content_type": "text/calendar", "size": 9, "content_id": "<cal@mailer>"},
                    {"id": "att_12", "content_type": "image/gif", "size": 9},
                    {"id": "att_tkt", "filename": "Electronic_ticket.pdf", "content_type": "application/pdf", "size": 3, "content_disposition": "attachment"},
                ],
            })))
            .mount(&server).await;
        let listing = |id: &str, name: Option<&str>, kind: &str| {
            let mut part = json!({"id": id, "size": 3, "content_type": kind, "download_url": format!("{}/dl/{id}", server.uri())});
            if let Some(name) = name {
                part["filename"] = json!(name);
            }
            part
        };
        Mock::given(method("GET")).and(path("/emails/receiving/re_1/attachments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": [
                listing("att_logo", Some("attachment-1.png"), "image/png"),
                listing("att_cal", Some("Add_to_your_calendar.ics"), "text/calendar"),
                // No `filename` key at all, which is how the part with no
                // name reaches us — `AttachmentMeta::filename` defaults.
                listing("att_12", None, "image/gif"),
                listing("att_tkt", Some("Electronic_ticket.pdf"), "application/pdf"),
            ]})))
            .mount(&server).await;
        for id in ["att_logo", "att_cal", "att_12", "att_tkt"] {
            Mock::given(method("GET")).and(path(format!("/dl/{id}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(b"%PDF".to_vec())).mount(&server).await;
        }
        Mock::given(method("POST")).and(path("/emails"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "sent_1"}))).mount(&server).await;

        let (_app, core, _dir) = test_app().await;
        open_round(&core, "autumn", 5).await;
        let a = admitted(&core, "111").await;
        scout_core::inbox::seed_email_identity_for_tests(&core, a, "me@example.com").await.unwrap();
        let mail_id = scout_core::inbox::record_mail(&core, a, a_mail("re_1")).await.unwrap().unwrap();
        let client = ResendClient::new(reqwest::Client::new(), "k".into(), server.uri());
        work_once(&core, &client, FROM, 10).await;

        let stored = scout_core::inbox::attachment_texts(&core, mail_id).await.unwrap();
        assert_eq!(
            stored.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            ["Electronic_ticket.pdf"],
            "the ticket was crowded out by the mail's own decoration"
        );
        // Not stored is also not downloaded: three fetches we do not make.
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.iter().filter(|r| r.url.path().starts_with("/dl/")).count(), 1);
        // The forward still carries what survived, and still goes.
        let forward: serde_json::Value = serde_json::from_slice(&reqs.iter().find(|r| r.url.path() == "/emails").unwrap().body).unwrap();
        assert_eq!(forward["attachments"][0]["filename"], "Electronic_ticket.pdf");
        assert_eq!(forward["attachments"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_booking_is_filed_on_a_draft_and_the_phone_hears_which() {
        let server = MockServer::start().await;
        resend_like(&server).await;
        model_saying(&server, r#"{"booking":true,"kind":"stay","title":"Hotel Lisboa","place":"Lisbon","date":"2026-10-12","summary":"Hotel in Lisbon, 12 Oct"}"#).await;
        let (_app, core, _dir) = test_app_with_model(&server.uri()).await;
        open_round(&core, "autumn", 5).await;
        let a = admitted(&core, "111").await;
        core.note_address(111, "telegram", "12345".into()).await.unwrap();
        scout_core::inbox::seed_email_identity_for_tests(&core, a, "me@example.com").await.unwrap();
        scout_core::inbox::record_mail(&core, a, a_mail("re_1")).await.unwrap().unwrap();
        let client = ResendClient::new(reqwest::Client::new(), "k".into(), server.uri());
        work_once(&core, &client, FROM, 10).await;
        let view = scout_core::inbox::view(&core, a, "goodscout.fyi").await.unwrap();
        assert_eq!(view.pending.len(), 1, "{view:?}");
        assert_eq!(view.pending[0].trip_name.as_deref(), Some("Lisbon, October"));
        assert!(view.other.is_empty());
        assert!(scout_core::inbox::mail_to_work(&core, 10).await.unwrap().is_empty(), "done, not retried");
        assert_eq!(forwards(&server.received_requests().await.unwrap()), 1);
        let queued = scout_core::mirror::pending(&core, 10).await.unwrap();
        assert_eq!(queued.iter().map(|q| q.body.as_str()).collect::<Vec<_>>(), ["A booking arrived and started a draft trip, Lisbon, October. Review it on goodscout.fyi/chat."]);
    }

    #[tokio::test]
    async fn a_round_trip_in_one_mail_is_two_bookings_on_one_trip() {
        let server = MockServer::start().await;
        resend_like(&server).await;
        model_saying(&server, r#"{"bookings":[{"booking":true,"kind":"flight","title":"AMS → HKG","place":"Hong Kong","origin":"AMS","destination":"HKG","date":"2026-11-02","confirmation_code":"KL7788","price":842.5,"currency":"EUR","summary":"AMS → HKG, 2 Nov"},{"booking":true,"kind":"flight","title":"HKG → AMS","place":"Amsterdam","origin":"HKG","destination":"AMS","date":"2026-11-23","confirmation_code":"KL7788","currency":"EUR","summary":"HKG → CDG → AMS, 23 Nov"}]}"#).await;
        let (_app, core, _dir) = test_app_with_model(&server.uri()).await;
        open_round(&core, "autumn", 5).await;
        let a = admitted(&core, "111").await;
        core.note_address(111, "telegram", "12345".into()).await.unwrap();
        scout_core::inbox::record_mail(&core, a, a_mail("re_1")).await.unwrap().unwrap();
        let client = ResendClient::new(reqwest::Client::new(), "k".into(), server.uri());
        work_once(&core, &client, FROM, 10).await;
        let view = scout_core::inbox::view(&core, a, "goodscout.fyi").await.unwrap();
        assert_eq!(view.pending.len(), 2, "{view:?}");
        let trips: std::collections::HashSet<Option<i64>> = view.pending.iter().map(|p| p.trip_id).collect();
        assert_eq!(trips.len(), 1, "one ticket, one journey, one draft");
        assert!(trips.iter().all(|t| t.is_some()), "and both legs were placed on it");
        assert_eq!(view.pending.iter().map(|p| p.price).collect::<Vec<_>>(), vec![Some(842.5), None], "the total once");
        let queued = scout_core::mirror::pending(&core, 10).await.unwrap();
        assert_eq!(
            queued.iter().map(|q| q.body.as_str()).collect::<Vec<_>>(),
            ["2 bookings arrived and started a draft trip, Hong Kong, November. Review them on goodscout.fyi/chat."]
        );
    }

    #[tokio::test]
    async fn without_an_address_to_forward_to_the_mail_is_still_read() {
        let server = MockServer::start().await;
        resend_like(&server).await;
        model_saying(&server, r#"{"booking":false,"summary":"A newsletter"}"#).await;
        let (_app, core, _dir) = test_app_with_model(&server.uri()).await;
        open_round(&core, "autumn", 5).await;
        let a = admitted(&core, "111").await;
        scout_core::inbox::record_mail(&core, a, a_mail("re_1")).await.unwrap().unwrap();
        let client = ResendClient::new(reqwest::Client::new(), "k".into(), server.uri());
        work_once(&core, &client, FROM, 10).await;
        let view = scout_core::inbox::view(&core, a, "goodscout.fyi").await.unwrap();
        assert_eq!(view.other[0].reason, "not_booking", "the model was asked");
        assert!(!view.other[0].forwarded, "nothing went, so nothing is recorded as gone");
        assert_eq!(forwards(&server.received_requests().await.unwrap()), 0);
    }

    fn extractions(reqs: &[wiremock::Request]) -> usize {
        reqs.iter().filter(|r| r.url.path() == "/chat/completions").count()
    }

    #[tokio::test]
    async fn a_forward_resend_was_not_there_for_is_sent_on_a_later_pass_without_a_second_reading() {
        let server = MockServer::start().await;
        // Failing first, so it wins the tie with `resend_like`'s POST, and
        // once only, so the second pass finds Resend back.
        Mock::given(method("POST")).and(path("/emails")).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).mount(&server).await;
        resend_like(&server).await;
        model_saying(&server, r#"{"booking":false,"summary":"A newsletter"}"#).await;
        let (_app, core, _dir) = test_app_with_model(&server.uri()).await;
        open_round(&core, "autumn", 5).await;
        let a = admitted(&core, "111").await;
        scout_core::inbox::seed_email_identity_for_tests(&core, a, "me@example.com").await.unwrap();
        let mail_id = scout_core::inbox::record_mail(&core, a, a_mail("re_1")).await.unwrap().unwrap();
        let client = ResendClient::new(reqwest::Client::new(), "k".into(), server.uri());
        work_once(&core, &client, FROM, 10).await;
        let view = scout_core::inbox::view(&core, a, "goodscout.fyi").await.unwrap();
        assert_eq!(view.other[0].reason, "not_booking", "the model was asked");
        assert!(!view.other[0].forwarded, "a 500 is not a forward");
        assert_eq!(forwards(&server.received_requests().await.unwrap()), 1, "it was tried");
        // Read, but not done: the forward is owed, and the row waits its turn.
        assert!(scout_core::inbox::mail_to_work(&core, 10).await.unwrap().is_empty(), "attempted a moment ago");
        scout_core::inbox::age_attempts_for_tests(&core, mail_id).await.unwrap();
        let due = scout_core::inbox::mail_to_work(&core, 10).await.unwrap();
        assert_eq!(due.iter().map(|m| (m.id, m.attempts, m.forwarded)).collect::<Vec<_>>(), vec![(mail_id, 1, false)]);
        work_once(&core, &client, FROM, 10).await;
        let view = scout_core::inbox::view(&core, a, "goodscout.fyi").await.unwrap();
        assert!(view.other[0].forwarded, "sent on the second pass");
        assert!(scout_core::inbox::mail_to_work(&core, 10).await.unwrap().is_empty(), "done");
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(forwards(&reqs), 2, "tried, then sent");
        assert_eq!(extractions(&reqs), 1, "read once");
        assert_eq!(reqs.iter().filter(|r| r.url.path() == "/emails/receiving/re_1").count(), 1, "fetched once");
        let forward: serde_json::Value = serde_json::from_slice(&reqs.iter().rfind(|r| r.url.path() == "/emails").unwrap().body).unwrap();
        assert_eq!(forward["text"], "Check-in 12 Oct", "the second send carries the stored body");
        assert_eq!(forward["attachments"][0]["filename"], "ticket.pdf");
    }

    #[tokio::test]
    async fn a_forward_resend_refuses_outright_is_not_tried_again() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/emails")).respond_with(ResponseTemplate::new(422)).mount(&server).await;
        resend_like(&server).await;
        model_saying(&server, r#"{"booking":false,"summary":"A newsletter"}"#).await;
        let (_app, core, _dir) = test_app_with_model(&server.uri()).await;
        open_round(&core, "autumn", 5).await;
        let a = admitted(&core, "111").await;
        scout_core::inbox::seed_email_identity_for_tests(&core, a, "me@example.com").await.unwrap();
        scout_core::inbox::record_mail(&core, a, a_mail("re_1")).await.unwrap().unwrap();
        let client = ResendClient::new(reqwest::Client::new(), "k".into(), server.uri());
        work_once(&core, &client, FROM, 10).await;
        let view = scout_core::inbox::view(&core, a, "goodscout.fyi").await.unwrap();
        assert_eq!((view.other[0].reason.as_str(), view.other[0].forwarded), ("not_booking", false));
        assert!(scout_core::inbox::mail_to_work(&core, 10).await.unwrap().is_empty(), "done: a 422 tomorrow is a 422");
    }

    #[tokio::test]
    async fn an_owed_forward_gives_up_after_the_attempts_and_the_row_says_so() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/emails")).respond_with(ResponseTemplate::new(503)).mount(&server).await;
        resend_like(&server).await;
        model_saying(&server, r#"{"booking":false,"summary":"A newsletter"}"#).await;
        let (_app, core, _dir) = test_app_with_model(&server.uri()).await;
        open_round(&core, "autumn", 5).await;
        let a = admitted(&core, "111").await;
        scout_core::inbox::seed_email_identity_for_tests(&core, a, "me@example.com").await.unwrap();
        let mail_id = scout_core::inbox::record_mail(&core, a, a_mail("re_1")).await.unwrap().unwrap();
        let client = ResendClient::new(reqwest::Client::new(), "k".into(), server.uri());
        for _ in 0..ATTEMPTS {
            work_once(&core, &client, FROM, 10).await;
            scout_core::inbox::age_attempts_for_tests(&core, mail_id).await.unwrap();
        }
        assert!(scout_core::inbox::mail_to_work(&core, 10).await.unwrap().is_empty(), "done after the last attempt");
        let view = scout_core::inbox::view(&core, a, "goodscout.fyi").await.unwrap();
        assert_eq!((view.other[0].reason.as_str(), view.other[0].forwarded), ("not_booking", false));
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(forwards(&reqs) as i64, ATTEMPTS);
        assert_eq!(extractions(&reqs), 1, "read once, however many times the forward was tried");
    }

    #[tokio::test]
    async fn an_outage_at_resend_spends_no_attempt_and_says_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/emails/receiving/re_1")).respond_with(ResponseTemplate::new(503)).mount(&server).await;
        let (_app, core, _dir) = test_app().await;
        open_round(&core, "autumn", 5).await;
        let a = admitted(&core, "111").await;
        core.note_address(111, "telegram", "12345".into()).await.unwrap();
        scout_core::inbox::seed_email_identity_for_tests(&core, a, "me@example.com").await.unwrap();
        let mail_id = scout_core::inbox::record_mail(&core, a, a_mail("re_1")).await.unwrap().unwrap();
        let client = ResendClient::new(reqwest::Client::new(), "k".into(), server.uri());
        // Longer than the mail has attempts: an outage is not the mail's fault.
        for pass in 0..ATTEMPTS + 2 {
            work_once(&core, &client, FROM, 10).await;
            assert!(scout_core::inbox::mail_to_work(&core, 10).await.unwrap().is_empty(), "pass {pass}: the spacing still applies");
            scout_core::inbox::age_attempts_for_tests(&core, mail_id).await.unwrap();
            let due = scout_core::inbox::mail_to_work(&core, 10).await.unwrap();
            assert_eq!(due.iter().map(|m| (m.id, m.attempts)).collect::<Vec<_>>(), vec![(mail_id, 0)], "pass {pass}: still due, nothing spent");
        }
        let view = scout_core::inbox::view(&core, a, "goodscout.fyi").await.unwrap();
        assert!(view.other.is_empty(), "not given up on");
        assert!(scout_core::mirror::pending(&core, 10).await.unwrap().is_empty(), "nothing to say yet");
        assert_eq!(forwards(&server.received_requests().await.unwrap()), 0);
    }

    #[tokio::test]
    async fn a_full_batch_is_followed_by_another_in_the_same_pass() {
        let server = MockServer::start().await;
        resend_like(&server).await;
        // Two mails, a batch of one: one pass drains both. The second
        // mail's record is not on the server, and a 404 is a reading
        // failure, which is fine — what is counted is that it was asked.
        let (_app, core, _dir) = test_app().await;
        open_round(&core, "autumn", 5).await;
        let a = admitted(&core, "111").await;
        scout_core::inbox::record_mail(&core, a, a_mail("re_1")).await.unwrap().unwrap();
        scout_core::inbox::record_mail(&core, a, a_mail("re_2")).await.unwrap().unwrap();
        let client = ResendClient::new(reqwest::Client::new(), "k".into(), server.uri());
        work_once(&core, &client, FROM, 1).await;
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.iter().filter(|r| r.url.path().starts_with("/emails/receiving/re_") && !r.url.path().ends_with("/attachments")).count(), 2);
    }
}
