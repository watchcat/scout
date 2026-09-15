//! `POST /inbound/resend`: Resend telling us a mail arrived for one of our
//! addresses.
//!
//! The webhook carries who wrote, to whom, the subject and the attachment
//! names — never the body; the worker fetches that later through the API.
//! So the only thing this route decides is whether the mail is somebody's:
//! the signature says Resend sent it, the `to` address says whose it is,
//! and one row goes into the store. Everything else is 200 and nothing,
//! because a 4xx makes Resend retry, and a retry cannot make an unknown
//! address known.

use crate::ratelimit::Limiter;
use crate::routes::auth::client_bucket;
use crate::AuthState;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use scout_core::core::Core;
use scout_core::inbox::{self, MailIn};
use serde::Deserialize;
use std::sync::Arc;

/// What the webhook handler needs, and nothing the signed-in half has: no
/// session key, no mailer, because this route never sees a person.
#[derive(Clone)]
pub(crate) struct InboundState {
    pub core: Arc<Core>,
    /// The `whsec_…` secret from Resend's dashboard.
    pub secret: String,
    /// The domain our addresses are on, lowercased.
    pub domain: String,
    /// Keyed on the caller's address. Generous, because Resend batches
    /// retries and a limit that turned real mail away would lose it; it
    /// is here so that a flood from anyone else costs one HMAC each and
    /// then nothing.
    pub by_ip: Arc<Limiter>,
}

/// The webhook's state out of the signed-in half's, when the deployment
/// has a webhook secret. `None` means the inbox is not set up, and no
/// route is mounted.
///
/// The secret is decoded once here and refused loudly if it cannot be:
/// a `whsec_` pasted without its prefix would otherwise fail every
/// webhook with a 401 and look, from outside, like an inbox nobody
/// writes to.
pub(crate) fn state_from(auth: &AuthState) -> Option<InboundState> {
    let secret = auth.cfg.resend_webhook_secret.clone()?;
    if let Err(err) = key_of(&secret) {
        tracing::warn!(%err, "RESEND_WEBHOOK_SECRET is not usable; the inbox stays off");
        return None;
    }
    Some(InboundState {
        core: auth.core.clone(),
        secret,
        domain: auth.cfg.inbox_domain.clone(),
        by_ip: Arc::new(Limiter::new(600, std::time::Duration::from_secs(60))),
    })
}

/// A webhook body is metadata for one mail: a few hundred bytes. The cap
/// keeps a stranger from making us buffer megabytes just to HMAC them.
const MOST_BODY_BYTES: usize = 256 * 1024;

pub(crate) fn routes(state: InboundState) -> Router {
    Router::new()
        .route("/inbound/resend", post(receive))
        .layer(DefaultBodyLimit::max(MOST_BODY_BYTES))
        .with_state(state)
}

/// How far a `svix-timestamp` may be from our clock, in either direction.
/// Svix's own libraries use five minutes; a replayed webhook older than
/// that is refused even with a valid signature.
pub(crate) const TOLERANCE_SECS: u64 = 300;

/// The key bytes behind `whsec_`.
fn key_of(secret: &str) -> anyhow::Result<Vec<u8>> {
    let encoded = secret
        .strip_prefix("whsec_")
        .ok_or_else(|| anyhow::anyhow!("the secret must start with whsec_"))?;
    Ok(base64::engine::general_purpose::STANDARD.decode(encoded)?)
}

/// The MAC over `{id}.{ts}.{body}`, keyed with the bytes behind `whsec_`.
/// One place for it so `sign` and `verify` cannot drift apart.
///
/// `ts` is the header's string, not a number: Svix signed the bytes it
/// sent, and a timestamp that parses to the same value but is spelled
/// differently is a different string under the MAC.
fn mac_for(secret: &str, id: &str, ts: &str, body: &str) -> anyhow::Result<Hmac<sha2::Sha256>> {
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&key_of(secret)?)?;
    mac.update(format!("{id}.{ts}.").as_bytes());
    mac.update(body.as_bytes());
    Ok(mac)
}

/// Svix's scheme, as Resend documents it: `base64(HMAC-SHA256(secret_bytes,
/// "{id}.{timestamp}.{body}"))`, the secret being the base64 after `whsec_`,
/// the header carrying one or more `v1,<sig>` entries. The raw body, byte
/// for byte: re-serialised JSON would not verify.
///
/// Test-only: in production Resend signs and we only ever check, so a
/// signer in the binary would be dead code with a key-shaped argument.
#[cfg(test)]
pub(crate) fn sign(secret: &str, id: &str, ts: i64, body: &str) -> anyhow::Result<String> {
    let mac = mac_for(secret, id, &ts.to_string(), body)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
}

/// Checks the three headers against the body. `now` is a parameter so the
/// tolerance can be tested without waiting five minutes.
///
/// The comparison is `Mac::verify_slice` on the decoded header, which is
/// constant-time; a hand-rolled compare of the base64 strings would leak
/// how many leading characters matched. Any `v1,` entry may match — Svix
/// sends several while a secret is being rotated.
pub(crate) fn verify(secret: &str, id: &str, ts: &str, signatures: &str, body: &str, now: i64) -> anyhow::Result<()> {
    // Parsed for the window only; the MAC is over the header as sent.
    // `abs_diff` rather than `(now - ts).abs()`: a header of `i64::MIN`
    // would make the subtraction overflow, which is a panic in a debug
    // build and a wrong answer in a release one.
    let ts_num: i64 = ts.parse().map_err(|_| anyhow::anyhow!("bad timestamp"))?;
    if now.abs_diff(ts_num) > TOLERANCE_SECS {
        anyhow::bail!("timestamp outside tolerance");
    }
    let mac = mac_for(secret, id, ts, body)?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let ok = signatures
        .split(' ')
        .filter_map(|e| e.strip_prefix("v1,"))
        .filter_map(|s| b64.decode(s).ok())
        .any(|given| mac.clone().verify_slice(&given).is_ok());
    if !ok {
        anyhow::bail!("no signature matched");
    }
    Ok(())
}

/// The envelope: the kind, and the data left unread until the kind says
/// it is ours. Every event Resend sends has a `data`, and none but
/// `email.received` has an `email_id` in it, so reading `Data` before
/// looking at `type` would make every other event a parse failure.
#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    data: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Data {
    email_id: String,
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: Vec<String>,
    #[serde(default)]
    received_for: Vec<String>,
    #[serde(default)]
    subject: Option<String>,
}

/// `addr` out of `Name <addr>`, or the string itself when there are no
/// angle brackets.
fn bare_address(raw: &str) -> &str {
    // Both from the right: a display name may itself contain a `<`, and
    // the address is always the last bracketed thing.
    match (raw.rfind('<'), raw.rfind('>')) {
        (Some(open), Some(close)) if open < close => &raw[open + 1..close],
        _ => raw,
    }
    .trim()
}

/// The local part of the first address whose domain is ours, as written:
/// `account_for_handle` does the normalising, so a capital here is its
/// business, not a reason to refuse.
fn our_handle(addresses: impl IntoIterator<Item = String>, domain: &str) -> Option<String> {
    // `sasha+hotel@…` is handed on whole; `normalise_handle` refuses the
    // `+`, so plus-addressing is an unknown handle rather than a feature.
    addresses.into_iter().find_map(|raw| {
        let (local, host) = bare_address(&raw).rsplit_once('@')?;
        (host.eq_ignore_ascii_case(domain) && !local.is_empty()).then(|| local.to_string())
    })
}

async fn receive(State(state): State<InboundState>, headers: HeaderMap, body: Bytes) -> Response {
    // The same bucketing as sign-in, shared fallback included: a request
    // with no forwarded address is counted with every other such
    // request rather than not at all. See `client_bucket` for why.
    if !state.by_ip.allow(&client_bucket(&headers)) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    let (Some(id), Some(ts), Some(sigs)) = (header("svix-id"), header("svix-timestamp"), header("svix-signature"))
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    // The bytes as they came, before any parsing: the signature is over
    // them and nothing else. JSON is UTF-8 by definition, so a body that
    // is not is not a webhook.
    let Ok(raw) = std::str::from_utf8(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if verify(&state.secret, &id, &ts, &sigs, raw, chrono::Utc::now().timestamp()).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let envelope: Envelope = match serde_json::from_str(raw) {
        Ok(e) => e,
        Err(err) => {
            // Signed by Resend and yet not the shape we know: worth a
            // line, and a 400 so their dashboard shows it too. The error
            // names a position, not the content.
            tracing::warn!(%err, "an inbound webhook did not parse");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    // A shared endpoint gets the sending side's events too. Not ours.
    if envelope.kind != "email.received" {
        return StatusCode::OK.into_response();
    }
    let data: Data = match envelope.data.map(serde_json::from_value).transpose() {
        Ok(Some(d)) => d,
        // Signed, ours by kind, and yet unreadable. A 400 would have
        // Resend retry the same bytes until it gave up; the line here is
        // what a person acts on instead.
        Ok(None) => {
            tracing::warn!("an email.received webhook carried no data");
            return StatusCode::OK.into_response();
        }
        Err(err) => {
            tracing::warn!(%err, "an email.received webhook's data did not parse");
            return StatusCode::OK.into_response();
        }
    };
    // `to` first, then `received_for`: a mail forwarded to us by someone
    // else's rule has our address only in the latter.
    let Some(handle) = our_handle(data.to.iter().chain(data.received_for.iter()).cloned(), &state.domain) else {
        return StatusCode::OK.into_response();
    };
    let account = match inbox::account_for_handle(&state.core, &handle).await {
        Ok(Some(a)) => a,
        Ok(None) => {
            // The handle and nothing else: who wrote to it and what about
            // is the mail's business, and this line is for spotting a
            // guessed address, not reading one. Bounded, because the
            // local part is whatever the sender typed.
            let shown: String = handle.chars().take(64).collect();
            tracing::info!(handle = shown, "mail for an address nobody holds");
            return StatusCode::OK.into_response();
        }
        Err(err) => {
            tracing::error!(%err, "looking up a handle");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    // No body yet: the webhook does not carry one, and the worker fetches
    // it through the API when it picks the row up.
    let mail = MailIn {
        provider_id: data.email_id,
        from: data.from,
        subject: data.subject,
        text: None,
        html: None,
        truncated: false,
    };
    match inbox::record_mail(&state.core, account, mail).await {
        // `None` is a redelivery already on file; 200 either way, or
        // Resend keeps sending it.
        Ok(_) => StatusCode::OK.into_response(),
        Err(err) => {
            tracing::error!(%err, "recording an inbound mail");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";

    fn now_secs() -> i64 {
        chrono::Utc::now().timestamp()
    }

    /// A router with the inbound route mounted and this secret, and a
    /// round open so `admitted` can admit somebody to own a handle.
    async fn inbound_app(secret: &str)
        -> (axum::Router, std::sync::Arc<scout_core::core::Core>, tempfile::TempDir)
    {
        let (app, core, dir) = test_app_with_inbox(secret).await;
        open_round(&core, "autumn", 5).await;
        (app, core, dir)
    }

    /// The pinned `email.received` payload with these two values.
    fn received_payload(email_id: &str, to: &str) -> String {
        serde_json::json!({
            "type": "email.received",
            "created_at": "2026-09-15T12:00:00.000Z",
            "data": {
                "email_id": email_id,
                "created_at": "2026-09-15T12:00:00.000Z",
                "from": "Hotel <hotel@example.com>",
                "to": [to],
                "cc": [],
                "bcc": [],
                "received_for": [to],
                "message_id": "<abc@example.com>",
                "subject": "Your booking",
                "attachments": [{"id": "att_1", "filename": "ticket.pdf", "content_type": "application/pdf", "content_disposition": "attachment", "content_id": null}]
            }
        })
        .to_string()
    }

    /// Posts the body with the three Svix headers computed from `secret`.
    async fn post_signed(app: &axum::Router, secret: &str, body: &str) -> axum::response::Response {
        let ts = now_secs();
        let sig = sign(secret, "msg_1", ts, body).unwrap();
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/inbound/resend")
                    .header("content-type", "application/json")
                    .header("svix-id", "msg_1")
                    .header("svix-timestamp", ts.to_string())
                    .header("svix-signature", format!("v1,{sig}"))
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[test]
    fn a_signature_verifies_only_with_the_right_secret_id_timestamp_and_body() {
        // Built the way Svix documents: base64(HMAC-SHA256(secret, "id.ts.body")).
        let secret = SECRET;   // any base64 after the prefix
        let (id, ts, body) = ("msg_1", now_secs(), r#"{"type":"email.received"}"#);
        let sig = sign(secret, id, ts, body).unwrap();
        assert!(verify(secret, id, &ts.to_string(), &format!("v1,{sig}"), body, now_secs()).is_ok());
        assert!(verify(secret, id, &ts.to_string(), &format!("v1,{sig}"), "{}", now_secs()).is_err(), "body changed");
        assert!(verify("whsec_AAAA", id, &ts.to_string(), &format!("v1,{sig}"), body, now_secs()).is_err(), "other secret");
        assert!(verify(secret, id, &ts.to_string(), &format!("v0,zzz v1,{sig}"), body, now_secs()).is_ok(), "one of several entries matches");
        assert!(verify(secret, id, &(ts - 600).to_string(), &format!("v1,{}", sign(secret, id, ts - 600, body).unwrap()), body, now_secs()).is_err(), "ten minutes old");
        // A header that is not base64 at all, or the wrong length, is a
        // mismatch rather than a panic.
        assert!(verify(secret, id, &ts.to_string(), "v1,!!!", body, now_secs()).is_err());
        assert!(verify(secret, id, &ts.to_string(), "v1,AAAA", body, now_secs()).is_err());
        assert!(verify(secret, "msg_2", &ts.to_string(), &format!("v1,{sig}"), body, now_secs()).is_err(), "other id");
        assert!(sign("nowhsec", id, ts, body).is_err(), "the prefix is part of the format");
        // A timestamp at either end of i64, or not a number at all, is a
        // refusal and not an overflow: `now - i64::MIN` does not fit.
        for bad in ["-9223372036854775808", "9223372036854775807", "abc", ""] {
            assert!(verify(secret, id, bad, &format!("v1,{sig}"), body, now_secs()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_timestamp_is_signed_as_the_header_spells_it() {
        // Svix signs the string it sends. A header that parses to the
        // same number but is spelled differently — a leading zero — is
        // still what the MAC was computed over, so re-serialising the
        // number would refuse a valid webhook.
        let (id, ts, body) = ("msg_1", now_secs(), r#"{"type":"email.received"}"#);
        let spelled = format!("0{ts}");
        let mac = mac_for(SECRET, id, &spelled, body).unwrap();
        let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        assert!(verify(SECRET, id, &spelled, &format!("v1,{sig}"), body, now_secs()).is_ok());
        assert!(verify(SECRET, id, &ts.to_string(), &format!("v1,{sig}"), body, now_secs()).is_err(), "the canonical spelling is a different string");
    }

    #[tokio::test]
    async fn the_webhook_stores_one_row_for_a_known_handle_and_nothing_otherwise() {
        let (app, core, _dir) = inbound_app(SECRET).await;
        let a = admitted(&core, "111").await;
        scout_core::inbox::set_handle(&core, a, "sasha").await.unwrap().unwrap();
        let body = received_payload("re_1", "sasha@goodscout.fyi");
        assert_eq!(post_signed(&app, SECRET, &body).await.status(), 200);
        assert_eq!(scout_core::inbox::mail_to_work(&core, 10).await.unwrap().len(), 1);
        assert_eq!(post_signed(&app, SECRET, &body).await.status(), 200, "a redelivery");
        assert_eq!(scout_core::inbox::mail_to_work(&core, 10).await.unwrap().len(), 1, "stored once");
        let stranger = received_payload("re_2", "nobody@goodscout.fyi");
        assert_eq!(post_signed(&app, SECRET, &stranger).await.status(), 200);
        assert_eq!(scout_core::inbox::mail_to_work(&core, 10).await.unwrap().len(), 1, "unknown handle dropped");
        assert_eq!(post_signed(&app, "whsec_AAAA", &body).await.status(), 401);
        let other = received_payload("re_3", "sasha@elsewhere.example");
        assert_eq!(post_signed(&app, SECRET, &other).await.status(), 200);
        assert_eq!(scout_core::inbox::mail_to_work(&core, 10).await.unwrap().len(), 1, "another domain is not ours");
    }

    #[tokio::test]
    async fn the_stored_row_carries_what_the_webhook_said_and_no_body() {
        let (app, core, _dir) = inbound_app(SECRET).await;
        let a = admitted(&core, "111").await;
        scout_core::inbox::set_handle(&core, a, "sasha").await.unwrap().unwrap();
        // A display name around the address, and a differently-cased
        // domain: both are the same mailbox.
        let body = received_payload("re_9", "Sasha Q <Sasha@GoodScout.fyi>");
        assert_eq!(post_signed(&app, SECRET, &body).await.status(), 200);
        let work = scout_core::inbox::mail_to_work(&core, 10).await.unwrap();
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].account_id, a);
        assert_eq!(work[0].provider_id, "re_9");
    }

    #[tokio::test]
    async fn headers_missing_is_400_and_other_events_are_ignored() {
        let (app, core, _dir) = inbound_app(SECRET).await;
        let a = admitted(&core, "111").await;
        scout_core::inbox::set_handle(&core, a, "sasha").await.unwrap().unwrap();

        let body = received_payload("re_1", "sasha@goodscout.fyi");
        let bare = app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/inbound/resend")
                    .header("content-type", "application/json")
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bare.status(), 400);
        assert_eq!(scout_core::inbox::mail_to_work(&core, 10).await.unwrap().len(), 0);

        // `email.sent` and friends are the sending side's events, which a
        // shared endpoint may well receive; they are not ours to store.
        let sent = body.replace("email.received", "email.sent");
        assert_eq!(post_signed(&app, SECRET, &sent).await.status(), 200);
        assert_eq!(scout_core::inbox::mail_to_work(&core, 10).await.unwrap().len(), 0);

        // Signed correctly but not JSON: our fault or theirs, either way
        // worth a retry rather than a silent drop.
        assert_eq!(post_signed(&app, SECRET, "not json").await.status(), 400);

        // An event of another kind whose `data` has no `email_id` at all
        // must not be a parse failure: it is not ours, so it is a 200.
        let domain = r#"{"type":"domain.created","created_at":"2026-09-15T12:00:00.000Z","data":{"id":"d_1","name":"goodscout.fyi"}}"#;
        assert_eq!(post_signed(&app, SECRET, domain).await.status(), 200);
        // And a received-mail event we cannot read is dropped, not
        // refused: a 400 would have Resend retry the same bytes forever.
        let no_id = r#"{"type":"email.received","data":{"from":"x@example.com","to":["sasha@goodscout.fyi"]}}"#;
        assert_eq!(post_signed(&app, SECRET, no_id).await.status(), 200);
        assert_eq!(scout_core::inbox::mail_to_work(&core, 10).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn a_body_over_the_limit_is_413() {
        let (app, _core, _dir) = inbound_app(SECRET).await;
        let body = format!(r#"{{"type":"email.received","pad":"{}"}}"#, "x".repeat(300 * 1024));
        assert_eq!(post_signed(&app, SECRET, &body).await.status(), 413);
    }

    /// `routes` over a state of the test's own, so the limiter can be
    /// small enough to fill.
    async fn small_limited_app(quota: usize)
        -> (axum::Router, std::sync::Arc<scout_core::core::Core>, tempfile::TempDir)
    {
        let (_app, core, dir) = inbound_app(SECRET).await;
        let state = InboundState {
            core: core.clone(),
            secret: SECRET.to_string(),
            domain: "goodscout.fyi".to_string(),
            by_ip: std::sync::Arc::new(Limiter::new(quota, std::time::Duration::from_secs(60))),
        };
        (routes(state), core, dir)
    }

    async fn post_signed_from(app: &axum::Router, body: &str, forwarded_for: Option<&str>) -> axum::response::Response {
        let ts = now_secs();
        let sig = sign(SECRET, "msg_1", ts, body).unwrap();
        let mut req = Request::builder()
            .method("POST")
            .uri("/inbound/resend")
            .header("content-type", "application/json")
            .header("svix-id", "msg_1")
            .header("svix-timestamp", ts.to_string())
            .header("svix-signature", format!("v1,{sig}"));
        if let Some(ff) = forwarded_for {
            req = req.header("x-forwarded-for", ff);
        }
        app.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap()
    }

    #[tokio::test]
    async fn one_address_is_limited_and_no_address_shares_a_bucket() {
        let (app, _core, _dir) = small_limited_app(2).await;
        let body = received_payload("re_1", "nobody@goodscout.fyi");
        assert_eq!(post_signed_from(&app, &body, Some("203.0.113.5")).await.status(), 200);
        assert_eq!(post_signed_from(&app, &body, Some("203.0.113.5")).await.status(), 200);
        assert_eq!(post_signed_from(&app, &body, Some("203.0.113.5")).await.status(), 429, "the quota is spent");
        assert_eq!(post_signed_from(&app, &body, Some("203.0.113.6")).await.status(), 200, "another address has its own");
        // No forwarded address at all: counted, in the one bucket every
        // such request shares — the crate's answer for the sign-in
        // limiter too, and for the same reason. Uncounted would be an
        // unmetered path the moment the proxy stopped setting the header.
        assert_eq!(post_signed_from(&app, &body, None).await.status(), 200);
        assert_eq!(post_signed_from(&app, &body, None).await.status(), 200);
        assert_eq!(post_signed_from(&app, &body, None).await.status(), 429);
    }

    #[tokio::test]
    async fn a_secret_that_cannot_be_decoded_leaves_the_inbox_off() {
        // Checked once at start-up rather than on every webhook, and
        // loudly: a deployment whose secret was pasted wrong must not
        // look like one whose inbox is merely quiet.
        let (_app, core, _dir) = test_app().await;
        let state_with = |secret: Option<&str>| {
            AuthState::new(
                crate::AuthConfig {
                    session_key: TEST_KEY.to_vec(),
                    bot_token: "123456:test-bot-token".to_string(),
                    resend_api_key: "test-key".to_string(),
                    mail_from: "Scout <hello@example.com>".to_string(),
                    base_url: "https://example.com".to_string(),
                    resend_webhook_secret: secret.map(str::to_string),
                    resend_base_url: "https://api.resend.com".to_string(),
                    inbox_domain: "goodscout.fyi".to_string(),
                },
                core.clone(),
            )
        };
        assert!(state_from(&state_with(Some(SECRET))).is_some());
        assert!(state_from(&state_with(None)).is_none());
        assert!(state_from(&state_with(Some("MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw"))).is_none(), "no prefix");
        assert!(state_from(&state_with(Some("whsec_!!!not base64"))).is_none(), "not base64");
    }

    #[tokio::test]
    async fn without_a_webhook_secret_there_is_no_route() {
        // `test_app` has no `RESEND_WEBHOOK_SECRET`. A route that accepted
        // mail with nothing to verify it against would let anyone file
        // rows for anyone.
        //
        // The `Origin` is there because the signed-in half's CSRF layer
        // sits over its 404 fallback too, and turns an origin-less `POST`
        // into a 400 before the fallback can say "no such route". With an
        // origin of our own the request gets through to the fallback, so
        // the 404 is a statement about the route and not about the layer.
        let (app, core, _dir) = test_app().await;
        open_round(&core, "autumn", 5).await;
        let a = admitted(&core, "111").await;
        scout_core::inbox::set_handle(&core, a, "sasha").await.unwrap().unwrap();
        let body = received_payload("re_1", "sasha@goodscout.fyi");
        let ts = now_secs();
        let sig = sign(SECRET, "msg_1", ts, &body).unwrap();
        let res = app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/inbound/resend")
                    .header("content-type", "application/json")
                    .header("origin", "https://example.com")
                    .header("svix-id", "msg_1")
                    .header("svix-timestamp", ts.to_string())
                    .header("svix-signature", format!("v1,{sig}"))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
        assert_eq!(scout_core::inbox::mail_to_work(&core, 10).await.unwrap().len(), 0);
    }

    #[test]
    fn the_handle_is_the_local_part_of_the_first_address_on_our_domain() {
        let ours = |addrs: &[&str]| {
            our_handle(addrs.iter().map(|s| s.to_string()), "goodscout.fyi")
        };
        assert_eq!(ours(&["sasha@goodscout.fyi"]).as_deref(), Some("sasha"));
        assert_eq!(ours(&["Sasha <SASHA@GoodScout.FYI>"]).as_deref(), Some("SASHA"));
        assert_eq!(ours(&["x@elsewhere.example", "sasha@goodscout.fyi"]).as_deref(), Some("sasha"));
        assert_eq!(ours(&["x@elsewhere.example"]), None);
        assert_eq!(ours(&["sasha@goodscout.fyi.evil.example"]), None, "a suffix is not our domain");
        assert_eq!(ours(&["nobody"]), None);
        assert_eq!(ours(&[]), None);
    }
}
