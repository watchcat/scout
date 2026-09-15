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

use crate::resend::{Outgoing, ResendClient};
use scout_core::core::Core;
use scout_core::inbox::{MailToWork, Placement};
use std::sync::Arc;
use std::time::Duration;

/// How many characters of body-plus-attachments the model is handed. A
/// booking confirmation is a page or two; past this it is a newsletter.
pub const TEXT_CAP: usize = 24_000;
/// How much of the body is kept on the row, so the page can show it. Wider
/// than `TEXT_CAP`: what the model does not need to read, a person may.
pub const BODY_CAP: usize = 512 * 1024;
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
        if let Err(e) = pass.await {
            tracing::error!(error = %e, "an inbox pass panicked");
        }
    }
}

/// One pass over due mail. Each step is idempotent per row: forwarding is
/// recorded on the row so a retry never forwards twice; attachments are
/// fetched only when the row has none yet.
pub async fn work_once(core: &Core, client: &ResendClient, from: &str, limit: usize) {
    let due = match scout_core::inbox::mail_to_work(core, limit).await {
        Ok(due) => due,
        Err(e) => {
            tracing::error!(error = %e, "could not read the inbox");
            return;
        }
    };
    for m in due {
        // Counted before the work, so a crash mid-way still spends one.
        if let Err(e) = scout_core::inbox::mail_attempted(core, m.id).await {
            tracing::warn!(error = %e, id = m.id, "inbox bookkeeping");
            continue;
        }
        // `process` marks the mail done itself, once the arrival is on
        // record; an `Ok` here means it did.
        let Err(e) = process(core, client, from, &m).await else { continue };
        let attempts = m.attempts + 1;
        tracing::warn!(error = %e, id = m.id, attempts, "an email could not be read");
        if attempts < ATTEMPTS {
            continue;
        }
        if let Err(e) = scout_core::inbox::mail_failed(core, m.id, &format!("{e:#}")).await {
            tracing::warn!(error = %e, id = m.id, "inbox bookkeeping");
        }
        let text = "An email arrived that Scout could not read. It is under Other mail on goodscout.fyi/chat.";
        if let Err(e) = scout_core::inbox::nudge(core, m.account_id, m.id, text).await {
            tracing::warn!(error = %e, id = m.id, "could not nudge about a failed email");
        }
    }
}

/// Everything one mail needs, in the order that makes a retry safe: body,
/// attachments, forward, then the model. The forward comes before the
/// model on purpose — the person gets their mail even when the model is
/// down — and a forward that fails is a warning, not a reason to leave
/// the mail unread: it is tried again on the next attempt, if there is
/// one, and shown as not forwarded either way.
async fn process(core: &Core, client: &ResendClient, from: &str, m: &MailToWork) -> anyhow::Result<()> {
    // 1. The body, from the API: the webhook carries none.
    let received = client.received(&m.provider_id).await?;
    scout_core::inbox::mail_body(core, m.id, received.text.clone(), received.html.clone(), BODY_CAP).await?;

    // 2. Attachments, once. A download that fails or is over the cap
    // leaves a row with a name and no bytes, so the page can still say
    // the mail had one.
    let stored = scout_core::inbox::attachment_texts(core, m.id).await?;
    let texts = if stored.is_empty() && !received.attachments.is_empty() {
        // The record already names the attachments; the list call is the
        // one that adds URLs, and most mail has nothing to list.
        let mut texts = Vec::new();
        for meta in client.attachments(&m.provider_id).await?.into_iter().take(ATTACHMENTS_PER_MAIL) {
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
            scout_core::inbox::store_attachment(core, m.id, &meta.filename, &meta.content_type, bytes, text.clone()).await?;
            texts.push((meta.filename, text));
        }
        texts
    } else {
        stored
    };

    // The record's sender is preferred over the webhook's copy on the
    // row: the same mail, seen in full rather than capped on the way in.
    let sender = if received.from.is_empty() { m.from.clone() } else { received.from.clone() };

    // 3. Forward, once. An account with no email identity has nowhere to
    // forward to; the mail is still read and filed, and a retry asks
    // again in case one was linked since.
    if !m.forwarded {
        if let Some(to) = scout_core::inbox::email_of(core, m.account_id).await? {
            let attachments = scout_core::inbox::attachment_bytes(core, m.id).await?;
            let mail = Outgoing {
                from: from.into(),
                to,
                reply_to: Some(sender.clone()),
                subject: received.subject.clone().or_else(|| m.subject.clone()).unwrap_or_default(),
                text: received.text.clone(),
                html: received.html.clone(),
                attachments,
            };
            match client.send(&mail).await {
                Ok(()) => scout_core::inbox::mail_forwarded(core, m.id).await?,
                Err(e) => tracing::warn!(error = %e, id = m.id, "the forward did not go; reading the mail anyway"),
            }
        }
    }

    // 4. Extract, place, record, and only then done.
    let parts: Vec<(&str, Option<&str>)> = texts.iter().map(|(f, t)| (f.as_str(), t.as_deref())).collect();
    let text = assemble(received.text.as_deref(), received.html.as_deref(), &parts, TEXT_CAP);
    let extraction = scout_core::inbox::extract(core, &text).await?;
    let (_, placement) = scout_core::inbox::record_arrival(core, m.account_id, m.id, extraction).await?;
    scout_core::inbox::mail_done(core, m.id).await?;

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
            arrival_nudge(p, name.as_deref().unwrap_or("a trip"))
        }
        // Not a booking, but from a place bookings come from: worth a line,
        // since the person may be waiting on it. Anything else is just
        // mail, and mail arrives quietly.
        None if looks_like_a_booking_site(&sender) => {
            format!("Mail from {}: \"{}\". Forwarded to you.", domain_of(&sender), m.subject.clone().unwrap_or_default())
        }
        None => return Ok(()),
    };
    if let Err(e) = scout_core::inbox::nudge(core, m.account_id, m.id, &text).await {
        tracing::warn!(error = %e, id = m.id, "could not nudge about an arrival");
    }
    Ok(())
}

/// The line on the phone for a booking that was placed: on a trip that
/// was already there, or on a draft made for it.
fn arrival_nudge(placement: Placement, name: &str) -> String {
    match placement {
        Placement::Trip(_) => format!("A booking arrived for {name}. Review it on goodscout.fyi/chat."),
        Placement::Draft(_) => {
            format!("A booking arrived and started a draft trip, {name}. Review it on goodscout.fyi/chat.")
        }
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
    fn the_nudge_says_whether_the_booking_joined_a_trip_or_started_one() {
        assert_eq!(arrival_nudge(Placement::Trip(1), "Lisbon"), "A booking arrived for Lisbon. Review it on goodscout.fyi/chat.");
        assert_eq!(
            arrival_nudge(Placement::Draft(1), "Lisbon, October"),
            "A booking arrived and started a draft trip, Lisbon, October. Review it on goodscout.fyi/chat."
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
        // Forwarded (one POST /emails), attachment stored, extraction failed on the closed port → attempts 1, still new.
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
        // A fourth pass finds nothing due.
        scout_core::inbox::age_attempts_for_tests(&core, mail_id).await.unwrap();
        work_once(&core, &client, FROM, 10).await;
        assert_eq!(server.received_requests().await.unwrap().iter().filter(|r| r.url.path() == "/emails/receiving/re_1").count(), 3);
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

    #[tokio::test]
    async fn a_forward_that_resend_refuses_does_not_stop_the_reading() {
        let server = MockServer::start().await;
        // Refusing first, so it wins the tie with `resend_like`'s POST.
        Mock::given(method("POST")).and(path("/emails")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
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
        assert_eq!(view.other[0].reason, "not_booking", "the model was asked");
        assert!(!view.other[0].forwarded, "a 500 is not a forward");
        assert_eq!(forwards(&server.received_requests().await.unwrap()), 1, "it was tried");
    }
}
