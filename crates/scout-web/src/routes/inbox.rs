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
use axum::routing::{delete, get, post};
use axum::Router;
use scout_core::inbox::{self, AddTarget, Claim, MailGone, Outcome};

pub fn routes(auth: AuthState) -> Router {
    Router::new()
        .route("/chat/inbox", get(view))
        .route("/chat/handle", post(set_handle))
        .route("/chat/handle/check", get(check_handle))
        .route("/chat/arrivals/{id}/add", post(add))
        .route("/chat/arrivals/{id}/ignore", post(ignore))
        .route("/chat/attachments/{id}", get(attachment))
        .route("/chat/mail/{id}", delete(delete_mail))
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
const STILL_WAITING: &str = "a booking from this email is still waiting";
const STILL_READING: &str = "Scout is still reading this email";

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

/// Forgets one Other-mail row now, rather than in the thirty days the
/// retention sweep would take.
///
/// `{}` on success, for the reason `ignore` gives. Both 409s name what is
/// in the way — a booking waiting on a decision, or a read still running —
/// because both are refusals the reader can act on by waiting or deciding,
/// unlike `DECIDED`, which is about the row they just pressed.
async fn delete_mail(
    State(auth): State<AuthState>,
    headers: HeaderMap,
    Path(mail_id): Path<i64>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return refused(StatusCode::BAD_REQUEST, "reload the page");
    }
    match inbox::delete_mail(&auth.core, account_id, mail_id).await {
        Ok(MailGone::Gone) => Json(serde_json::json!({})).into_response(),
        // A stranger's mail, one that never was, and one a second press
        // already took are the same answer, as in `add` and `ignore`.
        Ok(MailGone::NotFound) => refused(StatusCode::NOT_FOUND, NOT_YOURS),
        Ok(MailGone::Waiting) => refused(StatusCode::CONFLICT, STILL_WAITING),
        Ok(MailGone::Unsettled) => refused(StatusCode::CONFLICT, STILL_READING),
        Err(e) => {
            tracing::error!(error = %e, account_id, mail_id, "could not delete a mail");
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
/// it is shaped like one and is not one a browser would execute.
///
/// A ticket or a voucher is meant to be looked at, so a type the browser
/// renders in a viewer of its own goes out `inline` and opens in a tab
/// (see `INLINE`); everything else stays `attachment` and lands in
/// Downloads. What keeps a file that claims to be a PDF from being run as
/// a page under our origin is then three things rather than two:
/// `safe_mime` never names an executable type, `nosniff` stops the browser
/// guessing one, and the `inline` response carries `FILE_CSP`, under which
/// a document can do nothing even if it somehow became one.
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
            // The sanitised type decides both what is sent and whether it
            // opens, so it is computed once and the disposition reads it —
            // a second call on the raw `mime` would be an allowlist over a
            // stranger's string.
            let mime = safe_mime(&mime);
            let disposition = disposition_for(&mime);
            let mut response = bytes.into_response();
            let h = response.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(&mime).expect("a checked mime is header-safe"),
            );
            h.insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&content_disposition(disposition, &filename))
                    .expect("a sanitised and a percent-encoded name are both header-safe"),
            );
            if disposition == "inline" {
                // Only the response that actually renders needs this, and
                // it needs it instead of the site's: see `FILE_CSP`. The
                // shared layer defers to a policy a handler set, which is
                // what lets this survive being written here.
                h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(FILE_CSP));
            }
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
fn content_disposition(disposition: &str, name: &str) -> String {
    format!(
        "{disposition}; filename=\"{}\"; filename*=UTF-8''{}",
        safe_filename(name),
        encoded_filename(name)
    )
}

/// The types that open in a tab rather than land in Downloads, and the
/// only ones: a browser renders each of these in a viewer of its own — the
/// PDF reader, the image viewer, the plain-text pane — none of which
/// executes anything the file says, and none of which is the HTML parser.
///
/// The list is short on purpose. It is also *safely* short rather than
/// merely short, and the reason is one line up the call chain: the type
/// tested here has already been through `safe_mime`, which turns
/// `text/html`, `application/xhtml+xml`, `image/svg+xml`, both spellings
/// of JavaScript and both of XML into `application/octet-stream` (see
/// `ACTIVE`). So the types a browser would *execute* cannot reach this
/// list as themselves — they arrive as `application/octet-stream`, which
/// is not on it. Two independent conditions, and a test holds both: if
/// `safe_mime` is ever loosened, the allowlist is still the thing that
/// decides, and it names nothing executable.
const INLINE: [&str; 6] = [
    "application/pdf",
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "text/plain",
];

/// `inline` for a file the browser can render, `attachment` for the rest.
///
/// Takes the type *after* `safe_mime`, never the one the mail carried:
/// that is what makes `INLINE` an allowlist over a sanitised value rather
/// than over a stranger's string.
fn disposition_for(safe_mime: &str) -> &'static str {
    if INLINE.contains(&safe_mime) {
        "inline"
    } else {
        "attachment"
    }
}

/// What a rendered stranger's file is allowed to do: nothing.
///
/// The site policy allows `script-src 'self'`, which is right for our own
/// pages and wrong for somebody's forwarded ticket. `default-src 'none'`
/// means the document loads no subresource of any kind, and `sandbox` with
/// no tokens puts it in an opaque origin — no scripts, no forms, no
/// plugins, no same-origin access to anything of ours. A PDF or an image
/// needs none of that to be shown.
const FILE_CSP: &str = "default-src 'none'; sandbox";

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
    use super::{content_disposition, disposition_for, safe_filename, safe_mime, ACTIVE, INLINE};
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
            content_disposition("attachment", "my \"ticket\".pdf"),
            "attachment; filename=\"my_ticket_.pdf\"; filename*=UTF-8''my%20%22ticket%22.pdf"
        );
        assert_eq!(
            content_disposition("attachment", "билет.pdf"),
            "attachment; filename=\"_.pdf\"; filename*=UTF-8''%D0%B1%D0%B8%D0%BB%D0%B5%D1%82.pdf"
        );
        // Both names on `inline` too, and for the same two readers: they
        // are what the browser's own PDF viewer puts in the tab title and
        // hands to its save button, so a file that opens rather than
        // downloads must not lose the name it arrived with.
        assert_eq!(
            content_disposition("inline", "билет.pdf"),
            "inline; filename=\"_.pdf\"; filename*=UTF-8''%D0%B1%D0%B8%D0%BB%D0%B5%D1%82.pdf"
        );
        // The encoded original is capped at 200 bytes of the name before
        // encoding, on a character boundary.
        let long = "é".repeat(150);
        let header = content_disposition("attachment", &long);
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

    #[test]
    fn only_what_a_browser_renders_in_a_viewer_of_its_own_is_opened() {
        // Every type on the list, named one by one rather than looped over
        // `INLINE`: a test that iterates the constant it is checking agrees
        // with whatever someone adds to it, which is the one thing this
        // test exists to notice.
        for mime in [
            "application/pdf",
            "image/png",
            "image/jpeg",
            "image/gif",
            "image/webp",
            "text/plain",
        ] {
            assert_eq!(disposition_for(mime), "inline", "{mime}");
        }
        assert_eq!(INLINE.len(), 6, "a type was added to the allowlist without a reason above it");

        // And the kinds a browser cannot render, which is everything else:
        // a Word document, a zip, a type nobody claimed.
        for mime in [
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "application/zip",
            "application/octet-stream",
            "text/calendar",
            "video/mp4",
        ] {
            assert_eq!(disposition_for(mime), "attachment", "{mime}");
        }

        // What makes the allowlist safe rather than merely short: the types
        // a browser would *execute* never reach `disposition_for` as
        // themselves, because `safe_mime` has already turned them into
        // `application/octet-stream`. Asserted rather than asserted-in-
        // prose, because the day someone loosens `safe_mime` this is the
        // test that should go red.
        for mime in ACTIVE {
            assert_eq!(safe_mime(mime), "application/octet-stream", "{mime}");
            assert_eq!(disposition_for(&safe_mime(mime)), "attachment", "{mime}");
            assert!(!INLINE.contains(&mime), "{mime} is both active and inline");
        }
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

    /// A bodyless `DELETE` carrying the session cookie and — when given —
    /// the CSRF header. Its own helper because every other write on this
    /// router posts JSON: this route's whole request is its path.
    async fn delete_with_cookie(
        app: &axum::Router,
        uri: &str,
        session: &str,
        csrf: Option<&str>,
    ) -> axum::response::Response {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let mut request = Request::builder()
            .method("DELETE")
            .uri(uri)
            .header("origin", "https://example.com")
            .header("cookie", format!("{}={session}", crate::session::COOKIE));
        if let Some(csrf) = csrf {
            request = request.header("x-scout-csrf", csrf);
        }
        app.clone().oneshot(request.body(Body::empty()).unwrap()).await.unwrap()
    }

    /// The mail behind the one pending booking, as the page would read it.
    async fn pending_mail_id(app: &axum::Router, session: &str) -> i64 {
        let res = get_with_cookie(app, "/chat/inbox", session).await;
        let v: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        v["pending"][0]["mail_id"].as_i64().expect("a booking is waiting")
    }

    #[tokio::test]
    async fn other_mail_is_deleted_by_its_owner_and_a_waiting_booking_is_refused() {
        let (app, core, _dir) = inbox_app().await;
        let a = admitted(&core, "111").await;
        let plan = scout_core::trips::seed_trip_for_tests(&core, a, "Lisbon").await.unwrap();
        let arrival = scout_core::inbox::seed_arrival_for_tests(&core, a, "stay", "Hotel Alfama", "2026-10-12", Some(plan.trip.id)).await.unwrap();
        let (session, csrf) = signed_in(a);
        let mail = pending_mail_id(&app, &session).await;
        // Still waiting: deleting the mail would take the row off the
        // trip's timeline with nothing said about why.
        let res = delete_with_cookie(&app, &format!("/chat/mail/{mail}"), &session, Some(&csrf)).await;
        assert_eq!(res.status(), StatusCode::CONFLICT);
        assert_eq!(body_of(res).await, r#"{"ok":false,"reason":"a booking from this email is still waiting"}"#);
        // Decided, the same mail is an ordinary Other-mail row.
        let res = post_json_with_cookie(&app, &format!("/chat/arrivals/{arrival}/ignore"), &session, Some(&csrf), r#"{}"#).await;
        assert_eq!(res.status(), StatusCode::OK);
        let res = delete_with_cookie(&app, &format!("/chat/mail/{mail}"), &session, None).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "no CSRF header, no delete");
        let b = admitted(&core, "777").await;
        let (session_b, csrf_b) = signed_in(b);
        let res = delete_with_cookie(&app, &format!("/chat/mail/{mail}"), &session_b, Some(&csrf_b)).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "a stranger's mail is not there");
        assert_eq!(body_of(res).await, r#"{"ok":false,"reason":"not yours or not there"}"#);
        let res = get_with_cookie(&app, "/chat/inbox", &session).await;
        let v: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_eq!(v["other"].as_array().unwrap().len(), 1, "nothing refused took anything with it");
        let res = delete_with_cookie(&app, &format!("/chat/mail/{mail}"), &session, Some(&csrf)).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_of(res).await, "{}");
        let res = get_with_cookie(&app, "/chat/inbox", &session).await;
        let v: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert!(v["other"].as_array().unwrap().is_empty(), "off the page");
        // A second press from a tab that has not repainted yet.
        let res = delete_with_cookie(&app, &format!("/chat/mail/{mail}"), &session, Some(&csrf)).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_mail_still_being_read_is_refused_until_the_worker_is_done() {
        // No reading yet, so the pending-booking rule says nothing about
        // it; deleting it would strand the row the worker is about to
        // write against it.
        let (app, core, _dir) = inbox_app().await;
        let a = admitted(&core, "111").await;
        let mail = scout_core::inbox::seed_unread_mail_for_tests(&core, a).await.unwrap();
        let (session, csrf) = signed_in(a);
        let res = delete_with_cookie(&app, &format!("/chat/mail/{mail}"), &session, Some(&csrf)).await;
        assert_eq!(res.status(), StatusCode::CONFLICT);
        assert_eq!(body_of(res).await, r#"{"ok":false,"reason":"Scout is still reading this email"}"#);
    }

    #[tokio::test]
    async fn a_ticket_that_joined_a_trip_is_still_downloadable_once_its_mail_is_deleted() {
        let (app, core, _dir) = inbox_app().await;
        let a = admitted(&core, "111").await;
        let plan = scout_core::trips::seed_trip_for_tests(&core, a, "Lisbon").await.unwrap();
        let arrival = scout_core::inbox::seed_arrival_for_tests(&core, a, "stay", "Hotel Alfama", "2026-10-12", Some(plan.trip.id)).await.unwrap();
        let (session, csrf) = signed_in(a);
        let mail = pending_mail_id(&app, &session).await;
        let ticket = scout_core::inbox::seed_attachment_on_mail_for_tests(&core, mail, "ticket.pdf", "application/pdf", b"%PDF".to_vec()).await.unwrap();
        // Add is what moves the file from the mail onto the trip's item.
        let res = post_json_with_cookie(&app, &format!("/chat/arrivals/{arrival}/add"), &session, Some(&csrf), r#"{}"#).await;
        assert_eq!(res.status(), StatusCode::OK);
        let res = delete_with_cookie(&app, &format!("/chat/mail/{mail}"), &session, Some(&csrf)).await;
        assert_eq!(res.status(), StatusCode::OK);
        // The mail it arrived with is gone and the ticket is still served:
        // `attachment_owner` answers for it through the item now.
        let res = get_with_cookie(&app, &format!("/chat/attachments/{ticket}"), &session).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers()["content-disposition"].to_str().unwrap().contains("ticket.pdf"));
        assert_eq!(body_of(res).await, "%PDF");
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
            "inline; filename=\"my_ticket_.pdf\"; filename*=UTF-8''my%20%22ticket%22.pdf"
        );
        assert_eq!(res.headers()["x-content-type-options"], "nosniff");
        assert_eq!(res.headers()["cache-control"], "no-store");
        let page = scout_core::inbox::seed_attachment_for_tests(&core, a, "map.svg", "image/svg+xml", b"<svg/>".to_vec()).await.unwrap();
        let res = get_with_cookie(&app, &format!("/chat/attachments/{page}"), &session).await;
        assert_eq!(res.headers()["content-type"], "application/octet-stream", "never a type a browser would run");
    }

    #[tokio::test]
    async fn a_ticket_opens_in_the_browser_under_a_policy_of_its_own() {
        let (app, core, _dir) = inbox_app().await;
        let a = admitted(&core, "111").await;
        let id = scout_core::inbox::seed_attachment_for_tests(&core, a, "билет.pdf", "application/pdf", b"%PDF".to_vec()).await.unwrap();
        let (session, _) = signed_in(a);
        let res = get_with_cookie(&app, &format!("/chat/attachments/{id}"), &session).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["content-type"], "application/pdf");
        // Open it, and under both names: the ASCII one for every browser
        // that has ever read the header, the encoded one for the viewer's
        // title and its save button.
        assert_eq!(
            res.headers()["content-disposition"],
            "inline; filename=\"_.pdf\"; filename*=UTF-8''%D0%B1%D0%B8%D0%BB%D0%B5%D1%82.pdf"
        );
        // A stranger's file that renders gets a policy of its own, not the
        // site's — the site's allows `script-src 'self'`, which is the last
        // thing a rendered stranger's file should have. `sandbox` with no
        // token is an opaque origin: no scripts, no forms, no same-origin,
        // so it can load nothing and reach nothing of ours.
        assert_eq!(
            res.headers()["content-security-policy"],
            "default-src 'none'; sandbox",
            "the shared layer clobbered the handler's policy"
        );
        assert_eq!(res.headers()["x-content-type-options"], "nosniff");
        assert_eq!(res.headers()["cache-control"], "no-store");
        // Opening changes nothing about whose file it is.
        let b = admitted(&core, "777").await;
        let (session_b, _) = signed_in(b);
        assert_eq!(
            get_with_cookie(&app, &format!("/chat/attachments/{id}"), &session_b).await.status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn a_file_the_browser_cannot_render_is_still_saved() {
        let (app, core, _dir) = inbox_app().await;
        let a = admitted(&core, "111").await;
        let id = scout_core::inbox::seed_attachment_for_tests(
            &core,
            a,
            "voucher.docx",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            b"PK".to_vec(),
        )
        .await
        .unwrap();
        let (session, _) = signed_in(a);
        let res = get_with_cookie(&app, &format!("/chat/attachments/{id}"), &session).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            res.headers()["content-disposition"].to_str().unwrap().starts_with("attachment;"),
            "{:?}",
            res.headers()["content-disposition"]
        );
        assert!(res.headers()["content-disposition"].to_str().unwrap().contains("voucher.docx"));
        assert_eq!(res.headers()["x-content-type-options"], "nosniff");
        assert_eq!(res.headers()["cache-control"], "no-store");
    }

    #[tokio::test]
    async fn a_stored_page_is_neither_rendered_nor_offered_as_one() {
        // Belt and braces, and both halves pinned: `safe_mime` turns the
        // type into one no browser runs, and the disposition says save it.
        // Either alone would do; the point of asserting both is that
        // loosening one does not quietly become the only line of defence.
        let (app, core, _dir) = inbox_app().await;
        let a = admitted(&core, "111").await;
        let id = scout_core::inbox::seed_attachment_for_tests(&core, a, "itinerary.html", "text/html", b"<script>1</script>".to_vec()).await.unwrap();
        let (session, _) = signed_in(a);
        let res = get_with_cookie(&app, &format!("/chat/attachments/{id}"), &session).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["content-type"], "application/octet-stream");
        assert!(
            res.headers()["content-disposition"].to_str().unwrap().starts_with("attachment;"),
            "a stored page must never be opened: {:?}",
            res.headers()["content-disposition"]
        );
    }
}
