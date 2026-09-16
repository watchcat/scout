//! The inbox's signed-in surface: an address, and the three verbs on an
//! arrival.
//!
//! Everything the Trips tab needs to show forwarded bookings and let the
//! person add or ignore them. The reading of the mail happened in the
//! worker; nothing here calls a model or sends mail — every route is a
//! read or a decision over rows the store already holds.

use super::chat::{admitted_account, csrf_header_ok};
use super::sorry;
use crate::AuthState;
use axum::extract::{Json, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use scout_core::inbox::{self, AddTarget, Claim, Outcome};

pub fn routes(auth: AuthState) -> Router {
    Router::new()
        .route("/chat/inbox", get(view))
        .route("/chat/handle", post(set_handle))
        .route("/chat/handle/check", get(check_handle))
        .route("/chat/arrivals/{id}/add", post(add))
        .route("/chat/arrivals/{id}/ignore", post(ignore))
        .route("/chat/attachments/{id}", get(attachment))
        .layer(axum::middleware::from_fn_with_state(
            auth.clone(),
            super::only_from_our_own_pages,
        ))
        .with_state(auth)
}

/// Every refusal this module gives, in one shape: `{"ok":false,"reason"}`
/// under the status. The page has one fetch helper, and a helper that
/// reads a reason out of a 422 but an empty body out of a 409 is a helper
/// with a special case per status — the reason is the same kind of thing
/// whichever code carries it.
#[derive(serde::Serialize)]
struct Refused {
    ok: bool,
    reason: String,
}

fn refused(status: StatusCode, reason: &str) -> Response {
    (status, Json(Refused { ok: false, reason: reason.to_string() })).into_response()
}

const TAKEN: &str = "that one is taken";
const NOT_YOURS: &str = "not yours or not there";
const DECIDED: &str = "already decided";

/// The inbox as the tab draws it: the address, what is waiting, and the
/// mail that was not a booking.
async fn view(State(auth): State<AuthState>, headers: HeaderMap) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    match inbox::view(&auth.core, account_id, &auth.cfg.inbox_domain).await {
        Ok(view) => Json(view).into_response(),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not read the inbox");
            sorry()
        }
    }
}

#[derive(serde::Deserialize)]
struct CheckIn {
    handle: String,
}

/// The free answer. Its own struct rather than `Refused` with an option,
/// so it carries `handle` and no `reason: null`: the page reads `ok` and
/// then exactly one other field.
#[derive(serde::Serialize)]
struct Free {
    ok: bool,
    handle: String,
}

/// Whether `handle` could be claimed right now, for a form that asks as
/// the person types.
///
/// A `200` either way: "taken" is an answer, not a failure, and a page
/// that had to tell a 4xx apart from a network fault to draw a red line
/// would draw it wrong. Rate-limited on a budget of its own, sized for
/// keystrokes, because each check is a database read and the one route
/// that tells you whether a name is somebody's.
async fn check_handle(
    State(auth): State<AuthState>,
    headers: HeaderMap,
    Query(q): Query<CheckIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !auth.handle_by_account.allow(&account_id.to_string()) {
        return refused(StatusCode::TOO_MANY_REQUESTS, "slow down");
    }
    match inbox::check_handle(&auth.core, account_id, &q.handle).await {
        Ok(Claim::Claimed(handle)) => Json(Free { ok: true, handle }).into_response(),
        Ok(Claim::Invalid(reason)) => refused(StatusCode::OK, &reason),
        Ok(Claim::Taken) => refused(StatusCode::OK, TAKEN),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not check a handle");
            sorry()
        }
    }
}

#[derive(serde::Deserialize)]
struct HandleIn {
    handle: String,
}

#[derive(serde::Serialize)]
struct HandleOut {
    handle: String,
}

/// Claims the address. A rule broken is `422` with the sentence the live
/// check already showed; somebody else's is `409`.
async fn set_handle(
    State(auth): State<AuthState>,
    headers: HeaderMap,
    Json(body): Json<HandleIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return refused(StatusCode::BAD_REQUEST, "reload the page");
    }
    match inbox::set_handle(&auth.core, account_id, &body.handle).await {
        Ok(Claim::Claimed(handle)) => Json(HandleOut { handle }).into_response(),
        Ok(Claim::Invalid(reason)) => refused(StatusCode::UNPROCESSABLE_ENTITY, &reason),
        Ok(Claim::Taken) => refused(StatusCode::CONFLICT, TAKEN),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not set a handle");
            sorry()
        }
    }
}

/// Where the person asked an arrival to go, as the page spells it:
/// nothing for the trip the reading matched, `trip` naming one of theirs
/// the way every other route on the page names a trip, or `new: true`
/// for a draft.
///
/// Both fields are read loosely and checked by hand rather than typed,
/// so that `{"trip": 5}` or `{"new": "yes"}` gets this module's `422`
/// with a reason the page can show, not the extractor's plain-text one.
#[derive(serde::Deserialize)]
struct AddIn {
    #[serde(default)]
    trip: Option<serde_json::Value>,
    #[serde(default)]
    new: Option<serde_json::Value>,
}

const ADD_SHAPE: &str = "trip is a name, or new is true";

impl AddIn {
    fn target(&self) -> Option<AddTarget> {
        match (&self.trip, &self.new) {
            (None, None) => Some(AddTarget::Matched),
            (Some(serde_json::Value::String(name)), None) => Some(AddTarget::Named(name.clone())),
            (None, Some(serde_json::Value::Bool(true))) => Some(AddTarget::New),
            _ => None,
        }
    }
}

/// Adds the arrival to a trip and answers with that trip's plan — the
/// row `/chat/trips` would list, so the tab can repaint the one trip that
/// changed without a second load.
async fn add(
    State(auth): State<AuthState>,
    headers: HeaderMap,
    Path(arrival_id): Path<i64>,
    Json(body): Json<AddIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return refused(StatusCode::BAD_REQUEST, "reload the page");
    }
    let Some(target) = body.target() else {
        return refused(StatusCode::UNPROCESSABLE_ENTITY, ADD_SHAPE);
    };
    match inbox::add_arrival(&auth.core, account_id, arrival_id, target).await {
        Ok(Outcome::Done(plan)) => Json(plan).into_response(),
        // Somebody else's arrival and one that never existed are the same
        // answer, as everywhere on the signed-in half: a 403 would confirm
        // the row exists.
        Ok(Outcome::NotFound) => refused(StatusCode::NOT_FOUND, NOT_YOURS),
        Ok(Outcome::NotPending) => refused(StatusCode::CONFLICT, DECIDED),
        Err(e) => {
            tracing::error!(error = %e, account_id, arrival_id, "could not add an arrival");
            sorry()
        }
    }
}

/// Puts the arrival under Other mail. `{}` rather than `204`: every
/// answer this module gives is a JSON object, the refusals included, and
/// an empty body would be the one shape the page's fetch helper had to
/// special-case.
async fn ignore(
    State(auth): State<AuthState>,
    headers: HeaderMap,
    Path(arrival_id): Path<i64>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return refused(StatusCode::BAD_REQUEST, "reload the page");
    }
    match inbox::ignore_arrival(&auth.core, account_id, arrival_id).await {
        Ok(Outcome::Done(())) => Json(serde_json::json!({})).into_response(),
        // The same answer for a stranger and for nobody's, as in `add`.
        Ok(Outcome::NotFound) => refused(StatusCode::NOT_FOUND, NOT_YOURS),
        Ok(Outcome::NotPending) => refused(StatusCode::CONFLICT, DECIDED),
        Err(e) => {
            tracing::error!(error = %e, account_id, arrival_id, "could not ignore an arrival");
            sorry()
        }
    }
}

/// A file off a forwarded mail, for its owner.
///
/// The filename and the type both came in from a stranger's mail, so
/// neither is put on the wire as it arrived: the name is reduced to the
/// characters a header cannot be broken out of with (the real one rides
/// beside it, percent-encoded), and the type is passed through only when
/// it is shaped like one and is not one a browser would execute. `nosniff`
/// and `attachment` together are what keep a file that claims to be a PDF
/// from being run as a page under our origin.
///
/// The body is bounded by `inbox_worker::ATTACHMENT_CAP`, the most the
/// worker ever stored for one file.
async fn attachment(
    State(auth): State<AuthState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    match inbox::attachment_for(&auth.core, id, account_id).await {
        Ok(Some((filename, mime, bytes))) => {
            let mut response = bytes.into_response();
            let h = response.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(&safe_mime(&mime)).expect("a checked mime is header-safe"),
            );
            h.insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&content_disposition(&filename))
                    .expect("a sanitised and a percent-encoded name are both header-safe"),
            );
            h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
            // Said here as well as by the signed-half layer: a route that
            // serves somebody's ticket must not depend on where it is
            // mounted to stay out of a shared cache.
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Ok(None) => refused(StatusCode::NOT_FOUND, NOT_YOURS),
        Err(e) => {
            tracing::error!(error = %e, account_id, id, "could not read an attachment");
            sorry()
        }
    }
}

/// The characters a filename keeps as they are, in either form of the
/// header. Everything else becomes `_` in `filename=` and `%XX` in
/// `filename*=`.
fn plain(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// The longest ASCII form of the name can be, extension included.
const ASCII_NAME_CAP: usize = 60;
/// How much of the original name goes into `filename*=`, in bytes before
/// encoding: enough for any name a person would recognise, and a bound on
/// a header a stranger sized.
const REAL_NAME_CAP: usize = 200;

/// `attachment; filename="<ascii>"; filename*=UTF-8''<the real name>`.
///
/// Both forms because they serve different readers: `filename=` is what
/// every browser has always read and must be plain ASCII to be safe in
/// a quoted string, and `filename*=` (RFC 8187) is how a browser that
/// knows it restores a name written in another alphabet — which a booking
/// forwarded from a Portuguese hotel usually is.
fn content_disposition(name: &str) -> String {
    format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        safe_filename(name),
        encoded_filename(name)
    )
}

/// `[A-Za-z0-9._-]` of the name, anything else an underscore, runs of
/// them collapsed, capped at `ASCII_NAME_CAP` with the extension kept,
/// never empty. A stranger's filename is the one string on this route
/// that could carry a quote or a newline into a header.
fn safe_filename(name: &str) -> String {
    let mut safe = String::new();
    for c in name.chars() {
        if plain(c) {
            safe.push(c);
        } else if !safe.ends_with('_') {
            safe.push('_');
        }
    }
    if safe.is_empty() {
        return "file".to_string();
    }
    if safe.len() <= ASCII_NAME_CAP {
        return safe;
    }
    // The extension is what a desktop opens the file with, so the cut
    // comes out of the stem. An "extension" that is most of the name is
    // not one, and is cut like anything else.
    match safe.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() && ext.len() + 1 < ASCII_NAME_CAP / 2 => {
            format!("{}.{ext}", &stem[..ASCII_NAME_CAP - 1 - ext.len()])
        }
        _ => safe[..ASCII_NAME_CAP].to_string(),
    }
}

/// The original name, cut at `REAL_NAME_CAP` bytes on a character
/// boundary, percent-encoded byte by byte outside `[A-Za-z0-9._-]`.
fn encoded_filename(name: &str) -> String {
    let cut = name
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(name.len()))
        .take_while(|&i| i <= REAL_NAME_CAP)
        .last()
        .unwrap_or(0);
    let mut out = String::new();
    for &b in &name.as_bytes()[..cut] {
        if b.is_ascii() && plain(b as char) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The top-level types IANA registers. `chemical/x-pdb` and `x-world/
/// x-vrml` are things mail clients do put on the wire, and an unknown
/// kind is a type this route has no opinion about — which is what
/// `application/octet-stream` says.
const KINDS: [&str; 9] = [
    "application", "audio", "font", "image", "message", "model", "multipart", "text", "video",
];

/// Types a browser would execute rather than display. `attachment` and
/// `nosniff` should already keep them inert, and this is the third lock
/// on the same door: a stored SVG served as SVG is a script under our
/// origin the moment either of those is bypassed.
const ACTIVE: [&str; 7] = [
    "text/html",
    "application/xhtml+xml",
    "image/svg+xml",
    "text/javascript",
    "application/javascript",
    "text/xml",
    "application/xml",
];

/// The stored type when it is shaped like one — a registered kind, a
/// subtype in the characters a token allows — and is not one a browser
/// would run; else the type that promises nothing.
fn safe_mime(mime: &str) -> String {
    let lower = mime.trim().to_ascii_lowercase();
    let token = |s: &str| {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'+' | b'-'))
    };
    match lower.split_once('/') {
        Some((kind, sub)) if KINDS.contains(&kind) && token(sub) && !ACTIVE.contains(&lower.as_str()) => lower,
        _ => "application/octet-stream".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{content_disposition, safe_filename, safe_mime};
    use crate::tests::*;
    use axum::http::StatusCode;

    #[test]
    fn a_strangers_filename_cannot_break_out_of_the_header() {
        assert_eq!(safe_filename("ticket.pdf"), "ticket.pdf");
        assert_eq!(safe_filename("my ticket\"; x=\r\ny.pdf"), "my_ticket_x_y.pdf", "runs collapse");
        assert_eq!(safe_filename("билет.pdf"), "_.pdf");
        assert_eq!(safe_filename(""), "file");
        let long = format!("{}.pdf", "a".repeat(100));
        let capped = safe_filename(&long);
        assert_eq!(capped.len(), 60, "{capped}");
        assert!(capped.ends_with(".pdf"), "the extension survives the cap: {capped}");
        assert_eq!(safe_filename(&"b".repeat(100)).len(), 60, "no extension, still capped");
    }

    #[test]
    fn the_disposition_carries_an_ascii_name_and_the_real_one_encoded() {
        assert_eq!(
            content_disposition("my \"ticket\".pdf"),
            "attachment; filename=\"my_ticket_.pdf\"; filename*=UTF-8''my%20%22ticket%22.pdf"
        );
        assert_eq!(
            content_disposition("билет.pdf"),
            "attachment; filename=\"_.pdf\"; filename*=UTF-8''%D0%B1%D0%B8%D0%BB%D0%B5%D1%82.pdf"
        );
        // The encoded original is capped at 200 bytes of the name before
        // encoding, on a character boundary.
        let long = "é".repeat(150);
        let header = content_disposition(&long);
        let encoded = header.rsplit("''").next().unwrap();
        assert_eq!(encoded.len(), 100 * 6, "100 two-byte chars, 3 header bytes each: {header}");
    }

    #[test]
    fn a_strangers_mime_is_passed_through_only_when_it_is_shaped_like_one() {
        assert_eq!(safe_mime("Application/PDF"), "application/pdf");
        assert_eq!(safe_mime("image/svg+xml"), "application/octet-stream", "svg runs script");
        assert_eq!(safe_mime("text/html"), "application/octet-stream");
        assert_eq!(safe_mime("Application/XHTML+XML"), "application/octet-stream");
        assert_eq!(safe_mime("application/javascript"), "application/octet-stream");
        assert_eq!(safe_mime("text/xml"), "application/octet-stream");
        assert_eq!(safe_mime("chemical/x-pdb"), "application/octet-stream", "not an IANA kind");
        assert_eq!(safe_mime("image/png"), "image/png");
        assert_eq!(safe_mime("text/html; charset=utf-8"), "application/octet-stream");
        assert_eq!(safe_mime("pdf"), "application/octet-stream");
        assert_eq!(safe_mime("a/b\r\nX: y"), "application/octet-stream");
    }

    /// The inbox switched on — the same condition that mounts the webhook
    /// — with a round open so a sign-in admits.
    async fn inbox_app() -> (axum::Router, std::sync::Arc<scout_core::core::Core>, tempfile::TempDir) {
        let (app, core, dir) = test_app_with_inbox("whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").await;
        open_round(&core, "autumn", 5).await;
        (app, core, dir)
    }

    #[tokio::test]
    async fn without_the_webhook_there_is_no_inbox_to_sign_in_to() {
        // `test_app` sets no webhook secret: the feature is off, and the
        // page reads a 404 here as "no inbox", hiding the address and the
        // Other-mail section rather than showing an inbox nothing fills.
        let (app, core, _dir) = test_app_with_a_round().await;
        let a = admitted(&core, "111").await;
        let (session, csrf) = signed_in(a);
        assert_eq!(get_with_cookie(&app, "/chat/inbox", &session).await.status(), StatusCode::NOT_FOUND);
        assert_eq!(get_with_cookie(&app, "/chat/handle/check?handle=sasha", &session).await.status(), StatusCode::NOT_FOUND);
        let res = post_json_with_cookie(&app, "/chat/handle", &session, Some(&csrf), r#"{"handle":"sasha"}"#).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_handle_is_chosen_once_checked_live_and_shown_with_the_domain() {
        let (app, core, _dir) = inbox_app().await;
        let a = admitted(&core, "111").await;
        let (session, csrf) = signed_in(a);
        let res = get_with_cookie(&app, "/chat/handle/check?handle=Sasha", &session).await;
        assert_eq!(body_of(res).await, r#"{"ok":true,"handle":"sasha"}"#);
        let res = post_json_with_cookie(&app, "/chat/handle", &session, Some(&csrf), r#"{"handle":"Sasha"}"#).await;
        assert_eq!(res.status(), StatusCode::OK);
        let res = get_with_cookie(&app, "/chat/inbox", &session).await;
        let v: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_eq!(v["handle"], "sasha");
        assert_eq!(v["domain"], "goodscout.fyi");
        let res = get_with_cookie(&app, "/chat/handle/check?handle=postmaster", &session).await;
        assert!(body_of(res).await.contains("reserved"));
        let b = admitted(&core, "777").await;
        let (session_b, csrf_b) = signed_in(b);
        let res = post_json_with_cookie(&app, "/chat/handle", &session_b, Some(&csrf_b), r#"{"handle":"sasha"}"#).await;
        assert_eq!(res.status(), StatusCode::CONFLICT);
        assert_eq!(body_of(res).await, r#"{"ok":false,"reason":"that one is taken"}"#);
        let res = post_json_with_cookie(&app, "/chat/handle", &session_b, Some(&csrf_b), r#"{"handle":"ab"}"#).await;
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body_of(res).await, r#"{"ok":false,"reason":"a handle is 3 to 30 characters"}"#);
        let res = get_with_cookie(&app, "/chat/handle/check?handle=SASHA", &session).await;
        assert_eq!(body_of(res).await, r#"{"ok":true,"handle":"sasha"}"#, "your own handle is not taken from you");
        let res = get_with_cookie(&app, "/chat/handle/check?handle=sasha", &session_b).await;
        assert_eq!(body_of(res).await, r#"{"ok":false,"reason":"that one is taken"}"#);
    }

    #[tokio::test]
    async fn a_claim_without_the_csrf_header_is_refused() {
        let (app, core, _dir) = inbox_app().await;
        let a = admitted(&core, "111").await;
        let (session, _) = signed_in(a);
        let res = post_json_with_cookie(&app, "/chat/handle", &session, None, r#"{"handle":"sasha"}"#).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let res = get_with_cookie(&app, "/chat/inbox", &session).await;
        let v: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_eq!(v["handle"], serde_json::Value::Null, "nothing was claimed");
    }

    #[tokio::test]
    async fn the_live_check_has_a_budget_of_its_own() {
        let (app, core, _dir) = inbox_app().await;
        let a = admitted(&core, "111").await;
        let (session, _) = signed_in(a);
        for i in 0..60 {
            let res = get_with_cookie(&app, "/chat/handle/check?handle=sasha", &session).await;
            assert_eq!(res.status(), StatusCode::OK, "check {i}");
        }
        let res = get_with_cookie(&app, "/chat/handle/check?handle=sasha", &session).await;
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS, "the 61st in a minute");
        assert_eq!(body_of(res).await, r#"{"ok":false,"reason":"slow down"}"#);
        // Somebody else's minute is their own.
        let b = admitted(&core, "777").await;
        let (session_b, _) = signed_in(b);
        let res = get_with_cookie(&app, "/chat/handle/check?handle=sasha", &session_b).await;
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn an_arrival_is_added_to_its_trip_or_ignored_and_only_by_its_owner() {
        let (app, core, _dir) = inbox_app().await;
        let a = admitted(&core, "111").await;
        let plan = scout_core::trips::seed_trip_for_tests(&core, a, "Lisbon").await.unwrap();
        let arrival = scout_core::inbox::seed_arrival_for_tests(&core, a, "stay", "Hotel Alfama", "2026-10-12", Some(plan.trip.id)).await.unwrap();
        let (session, csrf) = signed_in(a);
        let res = get_with_cookie(&app, "/chat/inbox", &session).await;
        let v: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_eq!(v["pending"][0]["trip_name"], "Lisbon");
        let res = post_json_with_cookie(&app, &format!("/chat/arrivals/{arrival}/add"), &session, Some(&csrf), r#"{}"#).await;
        assert_eq!(res.status(), StatusCode::OK);
        let plan: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert!(plan["items"].as_array().unwrap().iter().any(|i| i["title"] == "Hotel Alfama" && i["booked"] == true));
        let res = post_json_with_cookie(&app, &format!("/chat/arrivals/{arrival}/add"), &session, Some(&csrf), r#"{}"#).await;
        assert_eq!(res.status(), StatusCode::CONFLICT, "already decided");
        assert_eq!(body_of(res).await, r#"{"ok":false,"reason":"already decided"}"#);
        let b = admitted(&core, "777").await;
        let other = scout_core::inbox::seed_arrival_for_tests(&core, a, "activity", "Museum", "2026-10-13", None).await.unwrap();
        let (session_b, csrf_b) = signed_in(b);
        let res = post_json_with_cookie(&app, &format!("/chat/arrivals/{other}/ignore"), &session_b, Some(&csrf_b), r#"{}"#).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(body_of(res).await, r#"{"ok":false,"reason":"not yours or not there"}"#);
        let res = post_json_with_cookie(&app, &format!("/chat/arrivals/{other}/add"), &session_b, Some(&csrf_b), r#"{}"#).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "a stranger cannot add it either");
        for bad in [r#"{"trip":5}"#, r#"{"new":false}"#, r#"{"new":"yes"}"#, r#"{"trip":"Lisbon","new":true}"#] {
            let res = post_json_with_cookie(&app, &format!("/chat/arrivals/{other}/add"), &session, Some(&csrf), bad).await;
            assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
            assert_eq!(body_of(res).await, r#"{"ok":false,"reason":"trip is a name, or new is true"}"#);
        }
        scout_core::trips::seed_trip_for_tests(&core, b, "Theirs").await.unwrap();
        let res = post_json_with_cookie(&app, &format!("/chat/arrivals/{other}/add"), &session, Some(&csrf), r#"{"trip":"Theirs"}"#).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "a stranger's trip name is not there");
        assert_eq!(body_of(res).await, r#"{"ok":false,"reason":"not yours or not there"}"#);
        let res = post_json_with_cookie(&app, &format!("/chat/arrivals/{other}/add"), &session, Some(&csrf), r#"{"new":true}"#).await;
        assert_eq!(res.status(), StatusCode::OK, "a new draft is made and kept for it");
        let museum = scout_core::inbox::seed_arrival_for_tests(&core, a, "activity", "Tram 28", "2026-10-14", None).await.unwrap();
        let res = post_json_with_cookie(&app, &format!("/chat/arrivals/{museum}/add"), &session, Some(&csrf), r#"{"trip":"lisbon"}"#).await;
        assert_eq!(res.status(), StatusCode::OK, "by name, as the page spells it");
        let plan: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_eq!(plan["name"], "Lisbon");
        assert!(plan["items"].as_array().unwrap().iter().any(|i| i["title"] == "Tram 28"));
        let res = post_json_with_cookie(&app, &format!("/chat/arrivals/{museum}/ignore"), &session, Some(&csrf), r#"{}"#).await;
        assert_eq!(res.status(), StatusCode::CONFLICT);
        assert_eq!(body_of(res).await, r#"{"ok":false,"reason":"already decided"}"#);
    }

    #[tokio::test]
    async fn an_attachment_is_served_to_its_owner_only() {
        let (app, core, _dir) = inbox_app().await;
        let a = admitted(&core, "111").await;
        let id = scout_core::inbox::seed_attachment_for_tests(&core, a, "ticket.pdf", "application/pdf", b"%PDF".to_vec()).await.unwrap();
        let (session, _) = signed_in(a);
        let res = get_with_cookie(&app, &format!("/chat/attachments/{id}"), &session).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["content-type"], "application/pdf");
        assert!(res.headers()["content-disposition"].to_str().unwrap().contains("ticket.pdf"));
        let b = admitted(&core, "777").await;
        let (session_b, _) = signed_in(b);
        assert_eq!(get_with_cookie(&app, &format!("/chat/attachments/{id}"), &session_b).await.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_strangers_name_and_type_reach_the_browser_inert() {
        let (app, core, _dir) = inbox_app().await;
        let a = admitted(&core, "111").await;
        let id = scout_core::inbox::seed_attachment_for_tests(&core, a, "my \"ticket\".pdf", "application/pdf", b"%PDF".to_vec()).await.unwrap();
        let (session, _) = signed_in(a);
        let res = get_with_cookie(&app, &format!("/chat/attachments/{id}"), &session).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()["content-disposition"],
            "attachment; filename=\"my_ticket_.pdf\"; filename*=UTF-8''my%20%22ticket%22.pdf"
        );
        assert_eq!(res.headers()["x-content-type-options"], "nosniff");
        assert_eq!(res.headers()["cache-control"], "no-store");
        let page = scout_core::inbox::seed_attachment_for_tests(&core, a, "map.svg", "image/svg+xml", b"<svg/>".to_vec()).await.unwrap();
        let res = get_with_cookie(&app, &format!("/chat/attachments/{page}"), &session).await;
        assert_eq!(res.headers()["content-type"], "application/octet-stream", "never a type a browser would run");
    }
}
