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
use scout_core::inbox::MailToWork;
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
pub async fn run(core: Arc<Core>, client: ResendClient, from: String) {
    loop {
        tokio::select! {
            _ = core.inbox_waiting() => {}
            _ = tokio::time::sleep(TICK) => {}
        }
        work_once(&core, &client, &from, BATCH).await;
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
        match process(core, client, from, &m).await {
            Ok(()) => {
                if let Err(e) = scout_core::inbox::mail_done(core, m.id).await {
                    tracing::warn!(error = %e, id = m.id, "inbox bookkeeping");
                }
            }
            Err(e) => {
                let attempts = m.attempts + 1;
                tracing::warn!(error = %e, id = m.id, attempts, "an email could not be read");
                if attempts >= ATTEMPTS {
                    if let Err(e) = scout_core::inbox::mail_failed(core, m.id, &format!("{e:#}")).await {
                        tracing::warn!(error = %e, id = m.id, "inbox bookkeeping");
                    }
                    let text = "An email arrived that Scout could not read. It is under Other mail on goodscout.fyi/chat.";
                    if let Err(e) = scout_core::inbox::nudge(core, m.account_id, m.id, text).await {
                        tracing::warn!(error = %e, id = m.id, "could not nudge about a failed email");
                    }
                }
            }
        }
    }
}

/// Everything one mail needs, in the order that makes a retry safe: body,
/// attachments, forward, then the model. The forward comes before the
/// model on purpose — the person gets their mail even when the model is
/// down, and a mail forwarded is a mail that no retry needs to forward.
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
            let text = match &bytes {
                Some(b) if meta.content_type == "application/pdf" => pdf_text(b.clone()).await,
                _ => None,
            };
            scout_core::inbox::store_attachment(core, m.id, &meta.filename, &meta.content_type, bytes, text.clone()).await?;
            texts.push((meta.filename, text));
        }
        texts
    } else {
        stored
    };

    // 3. Forward, once. An account with no email identity has nowhere to
    // forward to; the mail is still read and filed. The record's sender
    // and subject are preferred over the webhook's copy on the row, which
    // is the same mail seen earlier and capped on the way in.
    if !m.forwarded {
        if let Some(to) = scout_core::inbox::email_of(core, m.account_id).await? {
            let attachments = scout_core::inbox::attachment_bytes(core, m.id).await?;
            let sender = if received.from.is_empty() { m.from.clone() } else { received.from.clone() };
            client
                .send(&Outgoing {
                    from: from.into(),
                    to,
                    reply_to: Some(sender),
                    subject: received.subject.clone().or_else(|| m.subject.clone()).unwrap_or_default(),
                    text: received.text.clone(),
                    html: received.html.clone(),
                    attachments,
                })
                .await?;
        }
        scout_core::inbox::mail_forwarded(core, m.id).await?;
    }

    // 4. Extract, place, record, nudge.
    let parts: Vec<(&str, Option<&str>)> = texts.iter().map(|(f, t)| (f.as_str(), t.as_deref())).collect();
    let text = assemble(received.text.as_deref(), received.html.as_deref(), &parts, TEXT_CAP);
    let extraction = scout_core::inbox::extract(core, &text).await?;
    let (_, placement) = scout_core::inbox::record_arrival(core, m.account_id, m.id, extraction).await?;
    let nudge = match placement {
        Some(p) => {
            let name = scout_core::inbox::trip_name(core, m.account_id, p.id()).await?.unwrap_or_else(|| "a trip".into());
            format!("A booking arrived for {name}. Review it on goodscout.fyi/chat.")
        }
        // Not a booking, but from a place bookings come from: worth a line,
        // since the person may be waiting on it. Anything else is just
        // mail, and mail arrives quietly.
        None if looks_like_a_booking_site(&m.from) => {
            format!("Mail from {}: \"{}\". Forwarded to you.", domain_of(&m.from), m.subject.clone().unwrap_or_default())
        }
        None => return Ok(()),
    };
    scout_core::inbox::nudge(core, m.account_id, m.id, &nudge).await?;
    Ok(())
}

/// A PDF's text, or `None` when there is none to be had. Off the async
/// threads: extraction is CPU-bound and a big scan takes seconds. A panic
/// counts as no text — pdf-extract is known to panic on odd files, and an
/// odd attachment must not take the worker down.
async fn pdf_text(bytes: Vec<u8>) -> Option<String> {
    let extracted = tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pdf_extract::extract_text_from_mem(&bytes)))
    })
    .await;
    match extracted {
        Ok(Ok(Ok(text))) => Some(text.trim().to_string()).filter(|t| !t.is_empty()),
        _ => None,
    }
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
pub fn html_to_text(html: &str) -> String {
    let mut s = html.to_string();
    for tag in ["style", "script"] {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        while let Some(start) = s.to_lowercase().find(&open) {
            let end = s[start..].to_lowercase().find(&close).map(|e| start + e + close.len()).unwrap_or(s.len());
            s.replace_range(start..end, " ");
        }
    }
    let mut text = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => {
                in_tag = true;
                text.push(' ');
            }
            '>' => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
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

/// Senders whose mail is worth a line on the phone even when the model saw
/// no booking in it: a "your flight has changed" from one of these is
/// something the person is waiting on.
const BOOKING_DOMAINS: &[&str] = &[
    "booking.com", "expedia", "airbnb", "getyourguide", "tiqets", "trainline", "ryanair", "klm.com", "flytap",
    "easyjet", "transavia", "lufthansa", "vueling", "eurostar", "ns.nl", "hotels.com", "agoda", "hostelworld",
    "viator", "civitatis",
];

fn looks_like_a_booking_site(from: &str) -> bool {
    let d = domain_of(from);
    BOOKING_DOMAINS.iter().any(|b| d.contains(b))
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
    use crate::resend::ResendClient;
    use crate::tests::{admitted, open_round, test_app};
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
    }

    #[tokio::test]
    async fn garbage_is_not_a_pdf_and_says_so_without_panicking() {
        assert_eq!(pdf_text(b"not a pdf at all".to_vec()).await, None);
        assert_eq!(pdf_text(Vec::new()).await, None);
    }

    #[test]
    fn a_booking_site_is_known_by_the_domain_of_the_sender() {
        assert!(looks_like_a_booking_site("noreply@booking.com"));
        assert!(looks_like_a_booking_site("Ryanair <no-reply@mail.ryanair.com>"));
        assert!(!looks_like_a_booking_site("friend@example.com"));
        assert_eq!(domain_of("Hotel <hotel@Example.com>"), "example.com");
    }

    /// The four Resend calls the worker makes, as `resend.rs` mounts them.
    async fn resend_like(server: &MockServer) {
        Mock::given(method("GET")).and(path("/emails/receiving/re_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"re_1","from":"hotel@example.com","to":["sasha@goodscout.fyi"],"subject":"Your booking","text":"Check-in 12 Oct","html":"<p>Check-in 12 Oct</p>","attachments":[{"id":"att_1","filename":"ticket.pdf","content_type":"application/pdf","size":3}]})))
            .mount(server).await;
        Mock::given(method("GET")).and(path("/emails/receiving/re_1/attachments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object":"list","data":[{"id":"att_1","filename":"ticket.pdf","size":3,"content_type":"application/pdf","download_url":format!("{}/dl/att_1", server.uri()),"expires_at":"2026-09-15T13:00:00Z"}]})))
            .mount(server).await;
        Mock::given(method("GET")).and(path("/dl/att_1")).respond_with(ResponseTemplate::new(200).set_body_bytes(b"%PDF".to_vec())).mount(server).await;
        Mock::given(method("POST")).and(path("/emails"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"sent_1"}))).mount(server).await;
    }

    #[tokio::test]
    async fn one_pass_forwards_fetches_and_records_the_outcome() {
        let server = MockServer::start().await;
        resend_like(&server).await;
        // `Config::for_test` points the model at a closed port, so extraction
        // fails plainly and the rest of the pipeline is what is under test.
        let (_app, core, _dir) = test_app().await;
        open_round(&core, "autumn", 5).await;
        let a = admitted(&core, "111").await;
        scout_core::inbox::seed_email_identity_for_tests(&core, a, "me@example.com").await.unwrap();
        let mail_id = scout_core::inbox::record_mail(&core, a, MailIn { provider_id: "re_1".into(), from: "hotel@example.com".into(), subject: Some("Your booking".into()), text: None, html: None, truncated: false }).await.unwrap().unwrap();
        let client = ResendClient::new(reqwest::Client::new(), "k".into(), server.uri());
        work_once(&core, &client, "scout@send.goodscout.fyi", 10).await;
        // Forwarded (one POST /emails), attachment stored, extraction failed on the closed port → attempts 1, still new.
        let forwards = |reqs: &[wiremock::Request]| reqs.iter().filter(|r| r.url.path() == "/emails").count();
        assert_eq!(forwards(&server.received_requests().await.unwrap()), 1);
        let forward: serde_json::Value = serde_json::from_slice(&server.received_requests().await.unwrap().iter().find(|r| r.url.path() == "/emails").unwrap().body).unwrap();
        assert_eq!(forward["to"], json!(["me@example.com"]));
        assert_eq!(forward["reply_to"], json!(["hotel@example.com"]));
        assert_eq!(forward["attachments"][0]["filename"], "ticket.pdf");
        let stored = scout_core::inbox::attachment_texts(&core, mail_id).await.unwrap();
        assert_eq!(stored, vec![("ticket.pdf".to_string(), None)]);
        let view = scout_core::inbox::view(&core, a, "goodscout.fyi").await.unwrap();
        assert!(view.pending.is_empty() && view.other.is_empty(), "not decided yet");
        work_once(&core, &client, "scout@send.goodscout.fyi", 10).await;
        work_once(&core, &client, "scout@send.goodscout.fyi", 10).await;
        let view = scout_core::inbox::view(&core, a, "goodscout.fyi").await.unwrap();
        assert_eq!(view.other[0].reason, "failed", "three attempts, then Other mail");
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(forwards(&reqs), 1, "forwarded once, not per attempt");
        assert_eq!(reqs.iter().filter(|r| r.url.path() == "/dl/att_1").count(), 1, "attachments fetched once, not per attempt");
        // A fourth pass finds nothing due.
        work_once(&core, &client, "scout@send.goodscout.fyi", 10).await;
        assert_eq!(server.received_requests().await.unwrap().iter().filter(|r| r.url.path() == "/emails/receiving/re_1").count(), 3);
    }
}
