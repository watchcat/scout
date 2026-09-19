//! The inbox worker: every mail the webhook stored is fetched, forwarded,
//! read and filed here.
//!
//! The webhook (`inbound.rs`) stores an envelope and wakes this loop. For
//! each due mail the loop fetches the body from Resend, keeps the
//! attachments (a PDF's text pulled out for the model), forwards the whole
//! thing once to the address the account signed in with — unless the
//! account is who sent it, in which case the person has their own copy
//! already and only the reading is owed — asks the
//! tool-less extractor what it is, records the arrival where it belongs,
//! and says one line on Telegram. Every step is idempotent per row, so a
//! retry after a crash mid-way does the remaining work and repeats none
//! of the visible parts: the forward is recorded on the row, and the
//! attachments are fetched only while the row has none.

use crate::resend::{AttachmentMeta, Meta, Outgoing, ResendClient};
use scout_core::core::Core;
use scout_core::inbox::{MailPart, MailToWork, Placement};
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
    /// The account sent this mail itself — the person forwarded it out
    /// of their own inbox rather than a hotel writing to their booking
    /// address. Sending it on would hand them back a copy of what they
    /// had just sent. Settled, not owed: there is nothing to send later,
    /// and `forwarded_at` stays unset because nothing was sent.
    TheirOwn,
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
    // The record's own part list comes out with the body. It is no longer
    // the only thing that says which parts are furniture — the webhook
    // said so at ingest and the row keeps it — but Resend documents
    // neither field on this endpoint, so the record is the weaker source
    // and is read only for mail that predates the row. `parts_of` is
    // where the two meet; `winnow` is what does something with them.
    //
    // A retry still does without the record, deliberately: it reuses the
    // body off the row precisely so it does not depend on Resend being
    // up, and fetching the record only for the winnowing would spend that
    // call back. That path used to have no winnowing at all beyond the
    // name rule; now it winnows from the row, which costs it nothing.
    let (sender, subject, text, html, listed, record) = if m.text.is_some() || m.html.is_some() {
        (m.from.clone(), m.subject.clone(), m.text.clone(), m.html.clone(), true, Vec::new())
    } else {
        let r = client.received(&m.provider_id).await.map_err(resend)?;
        // The record's sender is stored with its body, so the passes
        // after this one read the same sender this one is about to judge.
        // Without that, a mail held back here for coming from the account
        // is forwarded by the next pass off the row's weaker copy, and the
        // row then says the opposite of what was decided.
        scout_core::inbox::mail_fetched(core, m.id, Some(r.from.clone()), r.text.clone(), r.html.clone(), BODY_CAP)
            .await
            .map_err(Failure::Reading)?;
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
        let all = client.attachments(&m.provider_id).await.map_err(resend)?;
        let listed_parts = all.len();
        let stored_parts = scout_core::inbox::mail_parts(core, m.id).await.map_err(Failure::Reading)?;
        if ids_never_met(&stored_parts, &all) {
            // Counts only, like the lines below. This is the one that says
            // the two calls number a part differently, which would make
            // everything above inert while looking like a quiet mail.
            tracing::info!(
                id = m.id,
                stored = stored_parts.len(),
                listed = listed_parts,
                "no part the webhook described was named in the attachment listing; nothing could be winnowed"
            );
        }
        let parts = parts_of(record, stored_parts);
        let (keep, skipped) = winnow(&parts, all);
        if skipped.any() {
            // Once per mail, counts only — a filename is the sender's
            // text. `info` rather than `debug`: the only subscriber in the
            // tree defaults to `info`, so a `debug` line is one production
            // never prints, and this is the line that turns "my ticket is
            // missing" into something answerable.
            tracing::info!(
                id = m.id,
                decoration = skipped.decoration,
                nameless = skipped.nameless,
                "parts left out of a mail: the body's own images, and parts with no filename"
            );
        }
        if keep.is_empty() && listed_parts > 0 {
            // Louder, because from the trip card a mail that lost its only
            // file is indistinguishable from a mail that never had one —
            // which is exactly the report that brought us here.
            tracing::warn!(id = m.id, listed = listed_parts, "every part of a mail was winnowed out; it will show as having no files");
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
    // Named in the nudge, so the Add button under it is a button for
    // something the reader can see. Taken before the readings move.
    let named: Vec<String> = readings.iter().filter(|e| e.booking).filter_map(|e| e.title.clone()).collect();
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
            arrival_nudge(p, name.as_deref().unwrap_or("a trip"), &named)
        }
        // A file handed to the bot in the chat is a question asked there,
        // and "nothing" is an answer it is owed.
        None if sender == scout_core::inbox::TELEGRAM_SENDER => {
            no_booking_in_document(subject.as_deref().unwrap_or("that file"))
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

/// The forward, unless the row says it went, or the account is who sent
/// it: a mail the person forwarded out of their own inbox is one they
/// already hold, and sending it on hands them a copy of what they just
/// sent. An account with no email identity has nowhere to forward to.
/// In both of those the mail is still read and filed.
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
    // Every address the account holds, of which the first is where a
    // forward goes — as it went when this read one address and no more.
    // The rest are here for the comparison below: a mail is the person's
    // own whichever of their addresses they sent it from, and only one
    // of those is the destination.
    let theirs = scout_core::inbox::emails_of(core, m.account_id).await.map_err(Failure::Reading)?;
    if scout_core::inbox::sender_is_the_account(sender, &theirs) {
        // The id and nothing else: the address this is about is the
        // person's own, and the log is not the place for it.
        tracing::info!(id = m.id, "a mail the account sent itself; reading it without sending it back");
        return Ok(Forwarded::TheirOwn);
    }
    let Some(to) = theirs.first().cloned() else {
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
    /// Parts marked as the mail's own furniture.
    decoration: usize,
    /// Parts with no filename, whatever else is said about them.
    nameless: usize,
}

impl Skipped {
    fn any(&self) -> bool {
        self.decoration + self.nameless > 0
    }
}

/// What one part of a mail says about itself, from whichever source said
/// it. Two can: the record `GET /emails/receiving/{id}` returns, which
/// documents neither field, and the row the webhook wrote at ingest,
/// which is where both are known to arrive. The rule below does not care
/// which, so it is written against this rather than against either.
///
/// The id and the two fields, and no filename: the attachment listing is
/// the authority on what a part is called, and a second copy here could
/// only disagree with it.
///
/// `Debug` for the tests' sake, as on `MailPart`, and on the same terms:
/// nothing here is a body or a filename, a Content-ID is still the
/// sender's text, and the lines this module writes carry counts of these
/// and never one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Part {
    id: String,
    content_disposition: Option<String>,
    content_id: Option<String>,
}

impl From<Meta> for Part {
    fn from(m: Meta) -> Self {
        Self { id: m.id, content_disposition: m.content_disposition, content_id: m.content_id }
    }
}

impl From<MailPart> for Part {
    fn from(p: MailPart) -> Self {
        Self { id: p.provider_id, content_disposition: p.content_disposition, content_id: p.content_id }
    }
}

/// Whether this mail's stored parts and its listing name nothing in
/// common — the one way this whole feature can be inert without anything
/// looking wrong.
///
/// The webhook and the attachment listing are two calls to Resend, and
/// nothing here can check that they number a part the same way; only
/// production ever sees both for one real mail. If they do not, every
/// stored part misses, `winnow` keeps everything, and the inbox looks
/// exactly as it did before the rule existed. That is the question the
/// card started from, so the answer belongs in the log rather than in
/// another round of guessing.
///
/// Both halves are required. A mail stored before the parts were kept has
/// nothing to match with, and a listing with nothing in it says nothing
/// about anyone's ids; neither is evidence of a disagreement.
fn ids_never_met(stored: &[MailPart], listed: &[AttachmentMeta]) -> bool {
    !stored.is_empty()
        && !listed.is_empty()
        && !stored.iter().any(|p| listed.iter().any(|a| a.id == p.provider_id))
}

/// The parts to winnow by: the received record's, with the store's laid
/// over them field by field.
///
/// Stored wins wherever it says anything, because it is the source we
/// have watched arrive — the webhook payload carries both fields per
/// part, and the record endpoint documents neither. The record is still
/// read, because a mail that arrived before the parts were kept has
/// nothing stored, and a rule that ignored the record would be no rule at
/// all for those.
///
/// Field by field rather than part by part, and this is the whole of the
/// care here. Every field of a stored part is optional: the webhook lists
/// a part whatever headers the mailer put on it. Replace the part
/// wholesale and a row that is silent about the disposition buries an
/// `attachment` the record did state, the Content-ID is left deciding,
/// and the ticket goes before its bytes are fetched.
///
/// And never the two verdicts together, in either direction: a part the
/// store calls `attachment` beside a record carrying only a Content-ID
/// keeps its file, because the stronger field is read from the stronger
/// source and the weaker one never gets a vote of its own. Unioning two
/// verdicts would drop that file, which is precisely the loss
/// `is_decoration` exists to avoid.
fn parts_of(record: Vec<Meta>, stored: Vec<MailPart>) -> Vec<Part> {
    // Keyed by id, which is the only thing that ties the two sources —
    // and ties either of them to the listing. A `BTreeMap` because
    // nothing downstream observes the order (`winnow` collects these into
    // a set), and one that is the same on every run is easier to read in
    // a debugger than one that is not.
    let mut by_id: std::collections::BTreeMap<String, Part> =
        record.into_iter().map(Part::from).map(|p| (p.id.clone(), p)).collect();
    for part in stored.into_iter().map(Part::from) {
        match by_id.get_mut(&part.id) {
            Some(known) => {
                known.content_disposition = part.content_disposition.or(known.content_disposition.take());
                known.content_id = part.content_id.or(known.content_id.take());
            }
            None => {
                by_id.insert(part.id.clone(), part);
            }
        }
    }
    by_id.into_values().collect()
}

/// The `Content-Disposition` type — the token before the first `;` —
/// lowercased, or `None` when the part states none. A real header is
/// `inline; filename="logo.png"`, so the parameters have to come off
/// before the token means anything; comparing the whole field made this
/// rule inert on most real mail.
fn disposition(m: &Part) -> Option<String> {
    let stated = m.content_disposition.as_deref()?;
    let token = stated.split(';').next().unwrap_or("").trim();
    // A field present but empty states nothing, which is not the same as
    // stating "attachment": it must not silence the Content-ID below.
    (!token.is_empty()).then(|| token.to_ascii_lowercase())
}

/// Whether a part is the mail's own furniture, by what it says of itself.
///
/// The part gets to say so itself where it says anything at all: an
/// explicit `attachment` is believed even when a Content-ID sits beside
/// it, because plenty of mailers stamp a Content-ID on every part they
/// send, the ticket included. Reading that stamp as decoration on its own
/// loses the ticket — and loses it before the bytes are ever fetched,
/// which is a worse failure than the crowding this exists to prevent.
///
/// Only when no disposition is stated does the Content-ID decide, and
/// then a non-blank one means the HTML body references this part by it:
/// a signature logo, a header banner, a tracking pixel.
/// One residual, chosen rather than overlooked: a real attachment that
/// states no disposition at all but carries a Content-ID is dropped. An
/// unstated disposition beside a Content-ID is the body-referenced image,
/// which is the case this exists for, so the weaker signal decides when
/// there is no stronger one. That residual is now bounded by where the
/// fields come from: `parts_of` prefers the row the webhook wrote, and
/// the webhook is known to send a disposition per part, so the case above
/// is the record's to produce and only for mail that predates the row.
fn is_decoration(m: &Part) -> bool {
    match disposition(m) {
        Some(token) => token == "inline",
        None => m.content_id.as_deref().is_some_and(|c| !c.trim().is_empty()),
    }
}

/// The listed parts worth keeping, and a count of what was left behind.
///
/// Two rules, and the order matters only to the counting:
///
/// A part `parts` marks as furniture goes — see `is_decoration` for what
/// marks it and why the part's own word wins, and `parts_of` for where
/// the two fields come from. A part that marks nothing survives, and so
/// does a mail nothing describes at all: that is the behaviour before
/// this rule existed, which is the safe way to be wrong. The listing is
/// the authority on which parts exist, and `parts` only on what they are
/// for, so a listed part nothing describes is kept.
///
/// A part with no filename goes regardless of what anything says about
/// it — neither source gets a vote against a part nobody can name. The
/// page renders it as "attachment 12" — its provider id — which tells the
/// reader nothing they can act on, and a real attachment has a name.
///
/// Winnowing happens before `ATTACHMENTS_PER_MAIL`, which is the point:
/// the cap must be spent on files, not on four logos and a ticket that
/// did not fit.
fn winnow(parts: &[Part], listed: Vec<AttachmentMeta>) -> (Vec<AttachmentMeta>, Skipped) {
    let decoration: std::collections::HashSet<&str> =
        parts.iter().filter(|m| is_decoration(m)).map(|m| m.id.as_str()).collect();
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
        // Where their own copy of their own mail is is not news to
        // them. The line itself stays: that Scout read it and saw no
        // booking is something the sender did not know either.
        Forwarded::TheirOwn | Forwarded::Refused | Forwarded::Later => {}
    }
    line
}

/// The line on the phone for the bookings that were placed: on a trip that
/// was already there, or on a draft made for them. `named` is what the
/// one mail confirmed, by title — a return ticket is two — and they share
/// a trip, so they share a line. The chat hangs Add and Ignore under it,
/// which is why the bookings are named: a button for a thing the reader
/// cannot see is a button they will not press.
fn arrival_nudge(placement: Placement, name: &str, named: &[String]) -> String {
    let (what, review) = if named.len() > 1 {
        (format!("{} bookings arrived", named.len()), "Add them here, or review")
    } else {
        ("A booking arrived".to_string(), "Add it here, or review")
    };
    let titles = if named.is_empty() { String::new() } else { format!(": {}", named.join(", ")) };
    match placement {
        Placement::Trip(_) => format!("{what} for {name}{titles}. {review} on goodscout.fyi/chat."),
        Placement::Draft(_) => format!("{what} and started a draft trip, {name}{titles}. {review} on goodscout.fyi/chat."),
    }
}

/// For a PDF sent to the bot that the extractor read and found no booking in.
fn no_booking_in_document(filename: &str) -> String {
    let filename: String = filename.chars().take(120).collect();
    format!("I read {filename} and found no booking in it. It is under Other mail on goodscout.fyi/chat for thirty days.")
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
pub async fn pdf_text(mail_id: i64, bytes: Vec<u8>) -> (Vec<u8>, Option<String>) {
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
    use scout_core::inbox::{MailIn, MailPart};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A part as `winnow` sees it, whichever source described it.
    fn part(id: &str, disposition: Option<&str>, cid: Option<&str>) -> Part {
        Part { id: id.into(), content_disposition: disposition.map(Into::into), content_id: cid.map(Into::into) }
    }

    /// The same part as the received record lists it, with the fields
    /// Resend documents on that endpoint and the two it does not.
    fn record_part(id: &str, filename: Option<&str>, disposition: Option<&str>, cid: Option<&str>) -> Meta {
        Meta {
            id: id.into(),
            filename: filename.map(Into::into),
            content_type: Some("application/octet-stream".into()),
            size: Some(1),
            content_disposition: disposition.map(Into::into),
            content_id: cid.map(Into::into),
        }
    }

    /// The same part as the webhook described it and the store kept it.
    fn stored_part(id: &str, disposition: Option<&str>, cid: Option<&str>) -> MailPart {
        MailPart {
            provider_id: id.into(),
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
    fn the_mails_decoration_is_left_off_and_a_source_that_says_nothing_leaves_everything_on() {
        // The four shapes one leg actually arrived with: a logo the body
        // draws, a calendar invite the mailer marked inline, a part sent
        // with no filename at all, and the one thing the traveller wants.
        let described = [
            part("att_logo", Some("inline"), Some("<logo@mailer>")),
            part("att_cal", Some("INLINE"), None),
            part("att_12", None, None),
            part("att_tkt", Some("attachment"), None),
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
        let (kept, skipped) = winnow(&described, all.clone());
        assert_eq!(kept.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["att_tkt"]);
        assert_eq!(skipped, Skipped { decoration: 2, nameless: 1 });

        // Parts that carry neither field — which is every part of a mail
        // that arrived before the webhook's list was kept, described only
        // by a record Resend documents neither field on — change nothing.
        // Only the part with no name goes, because a real attachment has
        // one.
        let silent: Vec<Part> = described.iter().map(|p| part(&p.id, None, None)).collect();
        let (kept, skipped) = winnow(&silent, all.clone());
        assert_eq!(kept.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["att_logo", "att_cal", "att_tkt"]);
        assert_eq!(skipped, Skipped { decoration: 0, nameless: 1 });

        // And nothing describing the parts at all — a mail stored before
        // this change, retried off its own body — is the same again.
        let (kept, _) = winnow(&[], all);
        assert_eq!(kept.len(), 3);
    }

    #[test]
    fn an_explicit_disposition_beats_a_content_id_and_inline_is_read_through_its_parameters() {
        // The defect this pins: plenty of mailers stamp a Content-ID on
        // every part, ticket included. Treating that as decoration on its
        // own loses the ticket — and loses it before the bytes are even
        // fetched, which is worse than the crowding the rule exists to
        // fix. A part that says what it is gets to say it.
        let ticket = part("att_tkt", Some("attachment"), Some("<tkt@mailer>"));
        let (kept, skipped) = winnow(&[ticket], vec![listed("att_tkt", "Electronic_ticket.pdf")]);
        assert_eq!(kept.len(), 1, "a stated `attachment` was overruled by a Content-ID");
        assert_eq!(skipped, Skipped::default());

        // And the other half: a real `Content-Disposition` carries its
        // parameters, so `inline` is the token before the first `;` and
        // never the whole field. Comparing the whole string made this
        // half inert on real mail, leaving the rule resting entirely on
        // the half above.
        let logo = part("att_logo", Some("inline; filename=\"logo.png\""), None);
        let (kept, skipped) = winnow(&[logo], vec![listed("att_logo", "logo.png")]);
        assert!(kept.is_empty(), "a parameterised `inline` was not recognised");
        assert_eq!(skipped, Skipped { decoration: 1, nameless: 0 });

        // Whitespace and case around the token are the sender's, not a
        // meaning. And an `inline` that also carries a Content-ID is
        // still inline — the two agree, so nothing subtle happens.
        for d in ["  INLINE  ", "Inline ;filename=x", "inline"] {
            let (kept, _) = winnow(&[part("a", Some(d), Some("<c@m>"))], vec![listed("a", "x.png")]);
            assert!(kept.is_empty(), "{d:?} is inline");
        }
    }

    #[test]
    fn a_content_id_only_decides_when_the_part_states_no_disposition() {
        // No disposition stated: the Content-ID is all there is, and a
        // part the HTML body references by one is a part the body draws.
        let (kept, _) = winnow(&[part("a", None, Some("<logo@mailer>"))], vec![listed("a", "logo.png")]);
        assert!(kept.is_empty());

        // An empty Content-ID is not a Content-ID. Neither is one of
        // spaces: both are a provider filling a field rather than saying
        // something, and neither may cost a traveller their ticket.
        for cid in ["", "   "] {
            let (kept, _) = winnow(&[part("a", None, Some(cid))], vec![listed("a", "t.pdf")]);
            assert_eq!(kept.len(), 1, "{cid:?} is not a Content-ID");
        }

        // A disposition stated but empty says nothing either, so the
        // Content-ID gets to decide after all.
        let (kept, _) = winnow(&[part("a", Some("  "), Some("<logo@m>"))], vec![listed("a", "logo.png")]);
        assert!(kept.is_empty(), "an empty disposition is not a statement");
    }

    #[test]
    fn what_a_part_says_of_itself_reads_the_same_from_either_source() {
        // Two shapes for one thing, under different names for the id. The
        // rule is written against neither, so both have to arrive at it
        // carrying the same two fields.
        let expected = part("att_1", Some("attachment"), Some("<tkt@mailer>"));
        assert_eq!(Part::from(record_part("att_1", Some("ticket.pdf"), Some("attachment"), Some("<tkt@mailer>"))), expected);
        assert_eq!(Part::from(stored_part("att_1", Some("attachment"), Some("<tkt@mailer>"))), expected);
    }

    #[test]
    fn the_stored_word_on_a_part_beats_the_records_and_is_never_added_to_it() {
        // Resend documents neither field on the received record and sends
        // both on the webhook, so where the two describe the same part the
        // stored one is the word we have actually watched arrive.
        //
        // Per part, and never the two verdicts together: a record that
        // carries a Content-ID and no disposition is the shape plenty of
        // mailers send, ticket included. Add that to the stored
        // `attachment` and the ticket goes — which is the loss this whole
        // rule was rewritten to avoid.
        let record = vec![record_part("att_tkt", Some("ticket.pdf"), None, Some("<tkt@mailer>"))];
        let stored = vec![stored_part("att_tkt", Some("attachment"), None)];
        let (kept, skipped) = winnow(&parts_of(record.clone(), stored), vec![listed("att_tkt", "ticket.pdf")]);
        assert_eq!(kept.len(), 1, "a stored `attachment` was overruled by the record's Content-ID");
        assert_eq!(skipped, Skipped::default());

        // With nothing stored for it the record still decides: a mail that
        // arrived before the parts were kept has only that to go on.
        let (kept, _) = winnow(&parts_of(record, vec![]), vec![listed("att_tkt", "ticket.pdf")]);
        assert!(kept.is_empty(), "the record was ignored for a part nothing else describes");

        // And the mirror of the case above, which is the one a per-part
        // merge gets wrong: the stored row is silent about a field the
        // record states. Every field of a stored part is optional — the
        // webhook omits what the mailer did not send — so "stored wins"
        // has to mean field by field. Replacing the whole part would let
        // the row's silence bury the record's explicit `attachment`,
        // leave the Content-ID deciding, and lose the ticket before its
        // bytes were ever fetched.
        let record = vec![record_part("att_tkt", Some("ticket.pdf"), Some("attachment"), None)];
        let stored = vec![stored_part("att_tkt", None, Some("<tkt@mailer>"))];
        let (kept, skipped) = winnow(&parts_of(record, stored), vec![listed("att_tkt", "ticket.pdf")]);
        assert_eq!(kept.len(), 1, "a stored silence buried the record's `attachment`");
        assert_eq!(skipped, Skipped::default());

        // And the preference is a preference, not a bias towards keeping:
        // the store is believed when it calls a part inline and the record
        // says nothing at all.
        let quiet = vec![record_part("att_logo", Some("logo.png"), None, None)];
        let parts = parts_of(quiet, vec![stored_part("att_logo", Some("inline"), None)]);
        let (kept, _) = winnow(&parts, vec![listed("att_logo", "logo.png")]);
        assert!(kept.is_empty());

        // A part only one source mentions is described by that source, and
        // a part neither mentions is kept.
        let parts = parts_of(
            vec![record_part("att_a", Some("a.png"), Some("inline"), None)],
            vec![stored_part("att_b", Some("inline"), None)],
        );
        let listing = vec![listed("att_a", "a.png"), listed("att_b", "b.png"), listed("att_c", "c.pdf")];
        let (kept, skipped) = winnow(&parts, listing);
        assert_eq!(kept.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["att_c"]);
        assert_eq!(skipped, Skipped { decoration: 2, nameless: 0 });
    }

    #[test]
    fn stored_parts_that_meet_none_of_the_listed_ones_are_worth_saying_out_loud() {
        // The whole rule rests on the webhook and the attachment listing
        // naming a part the same way, and nothing in this repo can check
        // that — the two calls are Resend's, and only production sees
        // both for one real mail. If they disagree the rule is inert and
        // silent, which is the same failure that brought the card here.
        // The `warn!` next door only fires when everything was winnowed
        // out; this is the other half.
        let stored = [stored_part("att_1", Some("inline"), None)];
        assert!(ids_never_met(&stored, &[listed("part-0", "logo.png")]), "two namings of one part");
        assert!(!ids_never_met(&stored, &[listed("att_1", "logo.png"), listed("att_2", "t.pdf")]), "one met");

        // Neither half of "no match" is worth a line on its own. A mail
        // stored before the parts were kept has nothing to match with,
        // and a listing with nothing in it says nothing about the ids.
        assert!(!ids_never_met(&[], &[listed("att_1", "logo.png")]));
        assert!(!ids_never_met(&stored, &[]));
    }

    #[test]
    fn a_part_the_record_does_not_mention_is_kept_and_so_is_everything_when_the_ids_match_nothing() {
        // The list endpoint is the authority on what exists; the record
        // is only the authority on what those parts are for. A part the
        // record skipped is a part we know nothing bad about.
        let described = [part("att_logo", Some("inline"), None)];
        let (kept, skipped) = winnow(&described, vec![listed("att_tkt", "Electronic_ticket.pdf")]);
        assert_eq!(kept.len(), 1);
        assert_eq!(skipped, Skipped::default());

        // A record whose ids line up with none of the listed parts — a
        // provider numbering the two calls differently — must not be read
        // as "all decoration" or as "all clean by luck". It is simply
        // silent, and silence keeps everything.
        let described = [part("part-0", Some("inline"), None), part("part-1", None, None)];
        let (kept, skipped) = winnow(&described, vec![listed("att_1", "logo.png"), listed("att_2", "t.pdf")]);
        assert_eq!(kept.len(), 2);
        assert_eq!(skipped, Skipped::default());
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
        // Silent about the forward, like the two below it: the person
        // sent this mail, so where their own copy is is not news. The
        // line itself stays — that Scout read it and saw no booking is.
        assert_eq!(other_mail_nudge("noreply@booking.com", "Changed", Forwarded::TheirOwn), "Mail from booking.com: \"Changed\".");
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
        let one = vec!["Hotel Alfama".to_string()];
        assert_eq!(
            arrival_nudge(Placement::Trip(1), "Lisbon", &one),
            "A booking arrived for Lisbon: Hotel Alfama. Add it here, or review on goodscout.fyi/chat."
        );
        assert_eq!(
            arrival_nudge(Placement::Draft(1), "Lisbon, October", &one),
            "A booking arrived and started a draft trip, Lisbon, October: Hotel Alfama. Add it here, or review on goodscout.fyi/chat."
        );
        // A round trip is two bookings off one mail, and the line counts
        // them rather than saying "a booking" twice or once.
        let two = vec!["AMS → LIS".to_string(), "LIS → AMS".to_string()];
        assert_eq!(
            arrival_nudge(Placement::Trip(1), "Lisbon", &two),
            "2 bookings arrived for Lisbon: AMS → LIS, LIS → AMS. Add them here, or review on goodscout.fyi/chat."
        );
        assert_eq!(
            arrival_nudge(Placement::Draft(1), "Hong Kong, November", &two),
            "2 bookings arrived and started a draft trip, Hong Kong, November: AMS → LIS, LIS → AMS. Add them here, or review on goodscout.fyi/chat."
        );
        // A reading with no title still gets a line, without a stray colon.
        assert_eq!(arrival_nudge(Placement::Trip(1), "Lisbon", &[]), "A booking arrived for Lisbon. Add it here, or review on goodscout.fyi/chat.");
        assert!(no_booking_in_document("eticket.pdf").starts_with("I read eticket.pdf and found no booking"));
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
        MailIn { provider_id: provider_id.into(), from: "hotel@example.com".into(), subject: Some("Your booking".into()), text: None, html: None, truncated: false, parts: Vec::new() }
    }

    /// A mail the webhook described part by part, as it does in
    /// production. `text` set is the other half of the retry path: it is
    /// what makes the worker read the body off the row instead of asking
    /// Resend for the record.
    fn a_mail_with_parts(provider_id: &str, text: Option<&str>, parts: Vec<MailPart>) -> MailIn {
        MailIn { text: text.map(Into::into), parts, ..a_mail(provider_id) }
    }

    fn forwards(reqs: &[wiremock::Request]) -> usize {
        reqs.iter().filter(|r| r.url.path() == "/emails").count()
    }

    /// The listing and the downloads for these parts, and a Resend that
    /// takes a forward. No received record: a test that wants one mounts
    /// its own.
    async fn listing_of(server: &MockServer, parts: &[(&str, &str)]) {
        let data: Vec<serde_json::Value> = parts
            .iter()
            .map(|(id, filename)| {
                json!({"id": id, "filename": filename, "size": 3, "content_type": "application/pdf",
                       "download_url": format!("{}/dl/{id}", server.uri())})
            })
            .collect();
        Mock::given(method("GET")).and(path("/emails/receiving/re_1/attachments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": data})))
            .mount(server).await;
        for (id, _) in parts {
            Mock::given(method("GET")).and(path(format!("/dl/{id}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(b"%PDF".to_vec())).mount(server).await;
        }
        Mock::given(method("POST")).and(path("/emails"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "sent_1"}))).mount(server).await;
    }

    /// An account that can be forwarded to, with `mail` on its inbox.
    async fn with_mail(core: &scout_core::core::Core, mail: MailIn) -> i64 {
        open_round(core, "autumn", 5).await;
        let a = admitted(core, "111").await;
        scout_core::inbox::seed_email_identity_for_tests(core, a, "me@example.com").await.unwrap();
        scout_core::inbox::record_mail(core, a, mail).await.unwrap().expect("a new mail")
    }

    #[tokio::test]
    async fn the_stored_parts_winnow_a_mail_whose_record_says_nothing_about_dispositions() {
        // The defect this closes. `GET /emails/receiving/{id}` documents
        // neither field, so the record here carries neither — which may
        // well be every record Resend sends, in which case the rule was
        // inert in production and the logo crowded the ticket out anyway.
        // The webhook did say, and what it said is on the row.
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/emails/receiving/re_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "re_1", "from": "airline@example.com", "subject": "Your booking", "text": "Row 12",
                "attachments": [
                    {"id": "att_logo", "filename": "logo.png", "content_type": "image/png", "size": 9},
                    {"id": "att_tkt", "filename": "ticket.pdf", "content_type": "application/pdf", "size": 3},
                ],
            })))
            .mount(&server).await;
        listing_of(&server, &[("att_logo", "logo.png"), ("att_tkt", "ticket.pdf")]).await;

        let (_app, core, _dir) = test_app().await;
        let mail_id = with_mail(&core, a_mail_with_parts("re_1", None, vec![
            stored_part("att_logo", Some("inline; filename=\"logo.png\""), Some("<logo@mailer>")),
            stored_part("att_tkt", Some("attachment"), None),
        ])).await;
        let client = ResendClient::new(reqwest::Client::new(), "k".into(), server.uri());
        work_once(&core, &client, FROM, 10).await;

        let stored = scout_core::inbox::attachment_texts(&core, mail_id).await.unwrap();
        assert_eq!(stored.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["ticket.pdf"]);
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.iter().filter(|r| r.url.path().starts_with("/dl/")).count(), 1, "the logo was fetched anyway");
    }

    #[tokio::test]
    async fn a_retry_that_reads_the_body_off_the_row_still_knows_which_part_is_decoration() {
        // The path that had no winnowing at all: the body is on the row,
        // so the worker deliberately does not fetch the record — the point
        // of storing the body is not to depend on Resend being up. The
        // parts are on the row too now, so the rule costs that path
        // nothing and applies to it all the same.
        let server = MockServer::start().await;
        listing_of(&server, &[("att_logo", "logo.png"), ("att_tkt", "ticket.pdf")]).await;

        let (_app, core, _dir) = test_app().await;
        let mail_id = with_mail(&core, a_mail_with_parts("re_1", Some("Row 12"), vec![
            stored_part("att_logo", Some("inline"), Some("<logo@mailer>")),
            stored_part("att_tkt", Some("attachment"), Some("<tkt@mailer>")),
        ])).await;
        let client = ResendClient::new(reqwest::Client::new(), "k".into(), server.uri());
        work_once(&core, &client, FROM, 10).await;

        let stored = scout_core::inbox::attachment_texts(&core, mail_id).await.unwrap();
        assert_eq!(stored.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["ticket.pdf"]);
        // And it is still a path that never asks Resend for the record —
        // no mock is mounted for it, so a request would be a 404 and the
        // mail would fail rather than quietly cost a call.
        let reqs = server.received_requests().await.unwrap();
        assert!(reqs.iter().all(|r| r.url.path() != "/emails/receiving/re_1"), "the retry path paid for a record");
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
    async fn the_per_mail_cap_is_spent_on_files_and_not_on_the_decoration_in_front_of_them() {
        // The one thing that pins the order the doc comment calls the
        // point. Six parts: four the body draws, then two tickets. Capped
        // first, the take of five would swallow the four logos and one
        // ticket and the second ticket would never be seen; winnowed
        // first, both tickets fit with room to spare.
        assert_eq!(ATTACHMENTS_PER_MAIL, 5, "this test is built around the cap's value");
        let server = MockServer::start().await;
        let decoration = ["att_a", "att_b", "att_c", "att_d"];
        let tickets = ["att_out", "att_back"];
        let mut record: Vec<serde_json::Value> = decoration
            .iter()
            .map(|id| json!({"id": id, "filename": format!("{id}.png"), "content_type": "image/png", "size": 9, "content_disposition": "inline; filename=\"logo.png\""}))
            .collect();
        // The tickets carry a Content-ID too, as a mailer that stamps one
        // on every part would. Their stated disposition is what counts.
        record.extend(tickets.iter().map(|id| {
            json!({"id": id, "filename": format!("{id}.pdf"), "content_type": "application/pdf", "size": 3,
                   "content_disposition": "attachment; filename=\"ticket.pdf\"", "content_id": format!("<{id}@mailer>")})
        }));
        Mock::given(method("GET")).and(path("/emails/receiving/re_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "re_1", "from": "airline@example.com", "subject": "Your booking",
                "text": "Row 12", "html": "<p>Row 12</p>", "attachments": record,
            })))
            .mount(&server).await;
        let all: Vec<serde_json::Value> = decoration
            .iter()
            .map(|id| (id, "png", "image/png"))
            .chain(tickets.iter().map(|id| (id, "pdf", "application/pdf")))
            .map(|(id, ext, kind)| json!({"id": id, "filename": format!("{id}.{ext}"), "size": 3, "content_type": kind,
                                          "download_url": format!("{}/dl/{id}", server.uri())}))
            .collect();
        Mock::given(method("GET")).and(path("/emails/receiving/re_1/attachments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": all})))
            .mount(&server).await;
        for id in decoration.iter().chain(tickets.iter()) {
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
            ["att_out.pdf", "att_back.pdf"],
            "the return ticket fell off the end of a cap spent on logos"
        );
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
        assert_eq!(queued.iter().map(|q| q.body.as_str()).collect::<Vec<_>>(), ["A booking arrived and started a draft trip, Lisbon, October: Hotel Lisboa. Add it here, or review on goodscout.fyi/chat."]);
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
            ["2 bookings arrived and started a draft trip, Hong Kong, November: AMS → HKG, HKG → AMS. Add them here, or review on goodscout.fyi/chat."]
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

    /// Resend as `resend_like` has it, with the mail coming from `from`
    /// rather than from a hotel. Mounted before it, so this record wins
    /// the tie for the one mail the two of them both describe.
    async fn resend_like_from(server: &MockServer, from: &str) {
        Mock::given(method("GET")).and(path("/emails/receiving/re_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "re_1", "from": from, "to": ["sasha@goodscout.fyi"], "subject": "Your booking",
                "text": "Check-in 12 Oct", "html": "<p>Check-in 12 Oct</p>",
                "attachments": [{"id": "att_1", "filename": "ticket.pdf", "content_type": "application/pdf", "size": 3}],
            })))
            .mount(server).await;
        resend_like(server).await;
    }

    #[tokio::test]
    async fn a_mail_the_person_sent_themselves_is_read_and_filed_and_never_handed_back() {
        let server = MockServer::start().await;
        // Their own address as a mailer writes it on the way out: a
        // display name, a capital, and the tag they file their bookings
        // under. Same mailbox, so this is Scout's own forward coming back.
        resend_like_from(&server, "Sasha Q <SASHA+hotels@Example.com>").await;
        model_saying(&server, r#"{"booking":false,"summary":"A newsletter"}"#).await;
        let (_app, core, _dir) = test_app_with_model(&server.uri()).await;
        open_round(&core, "autumn", 5).await;
        let a = admitted(&core, "111").await;
        // Two addresses, and the sender is the one a forward would not
        // have gone to: the comparison is against every identity the
        // account holds, not against the destination alone.
        scout_core::inbox::seed_email_identity_for_tests(&core, a, "a.work@elsewhere.example").await.unwrap();
        scout_core::inbox::seed_email_identity_for_tests(&core, a, "sasha@example.com").await.unwrap();
        // The webhook said the same sender the record does: it is one
        // From header read by two calls to Resend, and the row's copy is
        // what the drawn row is judged by while the worker prefers the
        // record's.
        let theirs = MailIn { from: "Sasha Q <SASHA+hotels@Example.com>".into(), ..a_mail("re_1") };
        let mail_id = scout_core::inbox::record_mail(&core, a, theirs).await.unwrap().unwrap();
        let client = ResendClient::new(reqwest::Client::new(), "k".into(), server.uri());
        work_once(&core, &client, FROM, 10).await;

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(forwards(&reqs), 0, "the person was sent back the mail they had just sent");
        // Everything else about the mail happens as it always did.
        let view = scout_core::inbox::view(&core, a, "goodscout.fyi").await.unwrap();
        assert_eq!(view.other[0].reason, "not_booking", "the model was still asked");
        assert!(!view.other[0].forwarded, "nothing was sent, so nothing is recorded as sent");
        assert!(view.other[0].sent_by_you, "and the row says why, rather than leaving it to read as a failed forward");
        assert_eq!(
            scout_core::inbox::attachment_texts(&core, mail_id).await.unwrap().iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["ticket.pdf"],
        );
        // Aged first, and only then asked: a row attempted a moment ago is
        // out of `mail_to_work` whatever `settle` decided, so asking
        // before the backoff had passed would have said "settled" about a
        // mail that was merely waiting.
        scout_core::inbox::age_attempts_for_tests(&core, mail_id).await.unwrap();
        assert!(
            scout_core::inbox::mail_to_work(&core, 10).await.unwrap().is_empty(),
            "held for a forward that is never coming"
        );
        // And a pass over it now finds nothing to do rather than sending late.
        work_once(&core, &client, FROM, 10).await;
        assert_eq!(forwards(&server.received_requests().await.unwrap()), 0);
    }

    #[tokio::test]
    async fn the_sender_the_record_named_is_the_one_every_later_pass_reads() {
        // The guarantee has to survive a retry, and the two passes read
        // the sender from different places: the first from the record it
        // fetched, every later one from the row. The webhook's `from` is
        // optional — a payload without the key stores an empty string —
        // so the row can be silent about a sender the record names, and
        // then a mail suppressed on the first pass is forwarded on the
        // second, with the row claiming the opposite of what was decided.
        let server = MockServer::start().await;
        resend_like_from(&server, "Sasha Q <sasha@example.com>").await;
        // The model fails once, which is what buys the second pass: the
        // mail is not settled, so it comes round again with its body — and
        // its sender — read off the row.
        Mock::given(method("POST")).and(path("/chat/completions")).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).mount(&server).await;
        model_saying(&server, r#"{"booking":false,"summary":"A newsletter"}"#).await;
        let (_app, core, _dir) = test_app_with_model(&server.uri()).await;
        open_round(&core, "autumn", 5).await;
        let a = admitted(&core, "111").await;
        scout_core::inbox::seed_email_identity_for_tests(&core, a, "sasha@example.com").await.unwrap();
        // The webhook said nothing about who wrote, as `Data::from`'s
        // default allows. Only the record knows, and only on pass one.
        let silent = MailIn { from: String::new(), ..a_mail("re_1") };
        let mail_id = scout_core::inbox::record_mail(&core, a, silent).await.unwrap().unwrap();
        let client = ResendClient::new(reqwest::Client::new(), "k".into(), server.uri());
        work_once(&core, &client, FROM, 10).await;
        assert_eq!(forwards(&server.received_requests().await.unwrap()), 0, "pass one held it back");

        scout_core::inbox::age_attempts_for_tests(&core, mail_id).await.unwrap();
        work_once(&core, &client, FROM, 10).await;
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(forwards(&reqs), 0, "the retry read a sender the first pass had already judged, and sent the mail anyway");
        assert_eq!(extractions(&reqs), 2, "and it was the reading that brought it round again");
        let view = scout_core::inbox::view(&core, a, "goodscout.fyi").await.unwrap();
        assert!(view.other[0].sent_by_you, "the row is drawn from the same sender the worker judged");
        assert!(!view.other[0].forwarded);
    }

    #[tokio::test]
    async fn a_sender_that_only_resembles_the_account_is_forwarded_like_any_stranger() {
        // The trap the rule is written around: a host that begins with
        // theirs is somebody else's host, and mail from it is mail they
        // would otherwise never see.
        let server = MockServer::start().await;
        resend_like_from(&server, "sasha@example.com.evil.example").await;
        model_saying(&server, r#"{"booking":false,"summary":"A newsletter"}"#).await;
        let (_app, core, _dir) = test_app_with_model(&server.uri()).await;
        open_round(&core, "autumn", 5).await;
        let a = admitted(&core, "111").await;
        scout_core::inbox::seed_email_identity_for_tests(&core, a, "sasha@example.com").await.unwrap();
        scout_core::inbox::record_mail(&core, a, a_mail("re_1")).await.unwrap().unwrap();
        let client = ResendClient::new(reqwest::Client::new(), "k".into(), server.uri());
        work_once(&core, &client, FROM, 10).await;

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(forwards(&reqs), 1);
        let forward: serde_json::Value = serde_json::from_slice(&reqs.iter().find(|r| r.url.path() == "/emails").unwrap().body).unwrap();
        assert_eq!(forward["to"], json!(["sasha@example.com"]));
        let view = scout_core::inbox::view(&core, a, "goodscout.fyi").await.unwrap();
        assert!(view.other[0].forwarded);
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
