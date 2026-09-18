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
    attachment_response(&auth, account_id, id).await
}

/// One account's file, as the response that serves it. Shared with the
/// Mini App's file links, which prove the account another way; everything
/// about how a stranger's file is sent stays in one place.
pub(crate) async fn attachment_response(auth: &AuthState, account_id: i64, id: i64) -> Response {
    match inbox::attachment_for(&auth.core, id, account_id).await {
        Ok(Some((filename, mime, bytes))) => {
            // The sanitised type decides both what is sent and whether it
            // opens, so it is computed once and the disposition reads it —
            // a second call on the raw `mime` would be an allowlist over a
            // stranger's string — and `SafeMime` is what stops the two
            // lines being written the other way round.
            let mime = safe_mime(&mime);
            let disposition = disposition_for(&mime);
            let mut response = bytes.into_response();
            let h = response.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(mime.as_str()).expect("a checked mime is header-safe"),
            );
            h.insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&content_disposition(disposition, &filename))
                    .expect("a sanitised and a percent-encoded name are both header-safe"),
            );
            if disposition == Disposition::Inline {
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
fn content_disposition(disposition: Disposition, name: &str) -> String {
    format!(
        "{}; filename=\"{}\"; filename*=UTF-8''{}",
        disposition.word(),
        safe_filename(name),
        encoded_filename(name)
    )
}

/// Render it, or save it.
///
/// An enum and not the word itself, because the word is written into a
/// header *and* decides whether the strict policy goes on the response. A
/// `&str` lets those two drift: `"Inline"` or `"inilne"` still produces a
/// header that looks plausible while the `== "inline"` beside it quietly
/// says no, and the file then renders under the site's policy. With two
/// variants the header and the decision cannot disagree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Disposition {
    Inline,
    Attachment,
}

impl Disposition {
    /// The word the header begins with — the one place it is spelled.
    fn word(self) -> &'static str {
        match self {
            Disposition::Inline => "inline",
            Disposition::Attachment => "attachment",
        }
    }
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

/// `Inline` for a file the browser can render, `Attachment` for the rest.
///
/// Takes a `SafeMime` rather than a `&str`, so the invariant the comment
/// above states is held by the compiler and not by the reader: the only
/// way to obtain one is `safe_mime`, so the raw type the mail carried
/// cannot be passed here at all. It used to be a `&str`, and swapping the
/// two lines in the handler left all the tests passing.
fn disposition_for(mime: &SafeMime) -> Disposition {
    if INLINE.contains(&mime.as_str()) {
        Disposition::Inline
    } else {
        Disposition::Attachment
    }
}

/// What a rendered stranger's file is allowed to do: almost nothing.
///
/// The site policy allows `script-src 'self'`, which is right for our own
/// pages and wrong for somebody's forwarded ticket. `default-src 'none'`
/// means the document loads no subresource of any kind, and `sandbox`
/// puts it in an opaque origin.
///
/// The sandbox is the load-bearing half, and for PDF specifically it is
/// the *only* thing in this response that does anything. Neither the type
/// allowlist nor `nosniff` touches what a PDF can do once the viewer has
/// it, and a PDF is not the inert picture it looks like: the format has
/// its own JavaScript subset that the engine implements, a `SubmitForm`
/// action that POSTs to an arbitrary URL, and link annotations that
/// navigate. A forwarded booking is a file a stranger chose, so all three
/// are reachable by whoever sent the mail. `sandbox` is what denies them.
/// Keep it — the reasoning that it is redundant with the allowlist is
/// wrong, and it is easy to arrive at. For the images and the plain text
/// it is defence in depth only.
///
/// `allow-downloads` because a bare sandbox also blocks a download the
/// document itself starts, and in the PDF viewer that is its own Save
/// button — the thing both filename forms on the header exist to title.
/// It grants an attacker nothing: whoever is looking at the file already
/// holds the bytes.
///
/// The deliberate cost: a link inside a real boarding pass — a hotel's
/// website, an airline's check-in page — will not open under this policy.
/// That is the price of the three capabilities above, paid knowingly, and
/// the reader still has the address in the mail the file came with.
/// `allow-downloads` is inert only because `allow-scripts`, `allow-forms`,
/// `allow-popups` and `allow-top-navigation` are all absent: with those
/// gone, the only actor left that can start a download is the reader
/// pressing Save on the document in front of them. The two tokens are one
/// decision. Add `allow-scripts` and this stops being a save button and
/// becomes a drive-by download from an opaque origin.
///
/// That escalation was held ready against the one thing this repo cannot
/// decide — whether a browser renders a sandboxed PDF at all, rather than
/// refusing it and leaving the reader with a blank tab. Checked in Chrome
/// on 2026-09-16: a forwarded ticket opens in a new tab and renders under
/// exactly this policy, so the escalation is not needed and must not be
/// made on a guess. Firefox and Safari are unverified; if one of them
/// comes back blank, the fix is that browser's viewer, not `allow-scripts`
/// for everyone.
const FILE_CSP: &str = "default-src 'none'; sandbox allow-downloads";

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
const ACTIVE: [&str; 12] = [
    "text/html",
    "application/xhtml+xml",
    "image/svg+xml",
    "text/javascript",
    "application/javascript",
    // The older and stranger spellings of the same two things. None of
    // them is on the inline list, so each was already served as a byte
    // stream and could not render — but this is the list that documents
    // what must never be a document, and a list that omits half the
    // spellings of JavaScript teaches the next reader the wrong rule.
    "application/x-javascript",
    "application/ecmascript",
    "text/ecmascript",
    "text/vbscript",
    "application/xslt+xml",
    "text/xml",
    "application/xml",
];

/// A media type that has been through `safe_mime`, and the only thing
/// this module will put in a `Content-Type` or judge a disposition from.
///
/// A newtype rather than a `String` so that "sanitised first" is a thing
/// the compiler holds. Both doc comments used to merely *say* it; a
/// reviewer swapped the two lines in the handler so the raw type decided,
/// and every test still passed.
#[derive(Debug)]
struct SafeMime(String);

impl SafeMime {
    fn as_str(&self) -> &str {
        &self.0
    }
}

/// The stored type when it is shaped like one — a registered kind, a
/// subtype in the characters a token allows — and is not one a browser
/// would run; else the type that promises nothing.
///
/// Parameters are cut off before any of that is judged, because real mail
/// sends them: `text/plain; charset=utf-8` is how almost every client
/// spells plain text, and a ticket often arrives as `application/pdf;
/// name="ticket.pdf"`. Judging the whole string meant both fell to
/// `application/octet-stream` — the plain text could never have matched
/// the inline list at all. Cutting first is also the safer reading, not
/// just the more useful one: `text/html; charset=utf-8` now meets the
/// `ACTIVE` list on its base type instead of being rejected incidentally,
/// because its parameter happened to contain characters a token forbids.
/// And nothing dangerous survives the cut — whatever followed the `;`,
/// including a `\r\n` somebody hoped to smuggle, is discarded rather than
/// inspected.
fn safe_mime(mime: &str) -> SafeMime {
    let lower = mime.trim().to_ascii_lowercase();
    let base = lower.split(';').next().unwrap_or("").trim();
    // 127 is the subtype's limit in RFC 6838, and a bound belongs here for
    // the same reason `safe_filename` has one: a stranger sizes this
    // header, and nothing downstream would refuse a three-hundred-letter
    // subtype that nothing can render anyway.
    let token = |s: &str| {
        !s.is_empty()
            && s.len() <= 127
            && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'+' | b'-'))
    };
    match base.split_once('/') {
        Some((kind, sub)) if KINDS.contains(&kind) && token(sub) && !ACTIVE.contains(&base) => {
            SafeMime(base.to_string())
        }
        _ => SafeMime("application/octet-stream".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        content_disposition, disposition_for, safe_filename, safe_mime, Disposition, ACTIVE, INLINE,
    };
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
            content_disposition(Disposition::Attachment, "my \"ticket\".pdf"),
            "attachment; filename=\"my_ticket_.pdf\"; filename*=UTF-8''my%20%22ticket%22.pdf"
        );
        assert_eq!(
            content_disposition(Disposition::Attachment, "билет.pdf"),
            "attachment; filename=\"_.pdf\"; filename*=UTF-8''%D0%B1%D0%B8%D0%BB%D0%B5%D1%82.pdf"
        );
        // Both names on `inline` too, and for the same two readers: they
        // are what the browser's own PDF viewer puts in the tab title and
        // hands to its save button, so a file that opens rather than
        // downloads must not lose the name it arrived with.
        assert_eq!(
            content_disposition(Disposition::Inline, "билет.pdf"),
            "inline; filename=\"_.pdf\"; filename*=UTF-8''%D0%B1%D0%B8%D0%BB%D0%B5%D1%82.pdf"
        );
        // The encoded original is capped at 200 bytes of the name before
        // encoding, on a character boundary.
        let long = "é".repeat(150);
        let header = content_disposition(Disposition::Attachment, &long);
        let encoded = header.rsplit("''").next().unwrap();
        assert_eq!(encoded.len(), 100 * 6, "100 two-byte chars, 3 header bytes each: {header}");
    }

    #[test]
    fn a_strangers_mime_is_passed_through_only_when_it_is_shaped_like_one() {
        assert_eq!(safe_mime("Application/PDF").as_str(), "application/pdf");
        assert_eq!(safe_mime("image/svg+xml").as_str(), "application/octet-stream", "svg runs script");
        assert_eq!(safe_mime("text/html").as_str(), "application/octet-stream");
        assert_eq!(safe_mime("Application/XHTML+XML").as_str(), "application/octet-stream");
        assert_eq!(safe_mime("application/javascript").as_str(), "application/octet-stream");
        assert_eq!(safe_mime("text/xml").as_str(), "application/octet-stream");
        assert_eq!(safe_mime("chemical/x-pdb").as_str(), "application/octet-stream", "not an IANA kind");
        assert_eq!(safe_mime("image/png").as_str(), "image/png");
        assert_eq!(safe_mime("pdf").as_str(), "application/octet-stream");
        assert_eq!(safe_mime("a/b\r\nX: y").as_str(), "application/octet-stream");

        // Parameters are cut before the type is judged, because real mail
        // sends them. These two spellings are what clients actually put on
        // the wire, and judging the whole string turned both into a byte
        // stream — plain text, which arrives with a charset essentially
        // always, could then never have matched the inline list.
        assert_eq!(safe_mime("text/plain; charset=utf-8").as_str(), "text/plain");
        assert_eq!(safe_mime("application/pdf; name=\"ticket.pdf\"").as_str(), "application/pdf");
        assert_eq!(safe_mime("Application/PDF ; Name=x").as_str(), "application/pdf", "space before the ;");
        // And the forced-inert list is met on the base type rather than by
        // the token rule happening to dislike the parameter.
        assert_eq!(safe_mime("text/html; charset=utf-8").as_str(), "application/octet-stream");
        assert_eq!(safe_mime("image/svg+xml; charset=utf-8").as_str(), "application/octet-stream");
        // Nothing after the `;` is inspected, so nothing after it can be
        // smuggled into a header either.
        assert_eq!(safe_mime("application/pdf;\r\nX: y").as_str(), "application/pdf");
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
            assert_eq!(disposition_for(&safe_mime(mime)), Disposition::Inline, "{mime}");
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
            assert_eq!(disposition_for(&safe_mime(mime)), Disposition::Attachment, "{mime}");
        }

        // What makes the allowlist safe rather than merely short: the types
        // a browser would *execute* never reach `disposition_for` as
        // themselves, because `safe_mime` has already turned them into
        // `application/octet-stream`. Asserted rather than asserted-in-
        // prose, because the day someone loosens `safe_mime` this is the
        // test that should go red.
        for mime in ACTIVE {
            assert_eq!(safe_mime(mime).as_str(), "application/octet-stream", "{mime}");
            assert_eq!(disposition_for(&safe_mime(mime)), Disposition::Attachment, "{mime}");
            assert!(!INLINE.contains(&mime), "{mime} is both active and inline");
        }

        // A type carrying a parameter is decided on its base, both ways
        // round: the ticket opens, the page does not.
        assert_eq!(
            disposition_for(&safe_mime("application/pdf; name=\"ticket.pdf\"")),
            Disposition::Inline
        );
        assert_eq!(
            disposition_for(&safe_mime("text/plain; charset=utf-8")),
            Disposition::Inline
        );
        assert_eq!(
            disposition_for(&safe_mime("text/html; charset=utf-8")),
            Disposition::Attachment
        );
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
    async fn the_trip_shows_the_ticket_its_added_booking_arrived_with() {
        // The file was visible on the dashed pending row. Pressing Add must
        // not be the moment it disappears: it is the same ticket, now on
        // the item the booking became.
        let (app, core, _dir) = inbox_app().await;
        let a = admitted(&core, "111").await;
        let plan = scout_core::trips::seed_trip_for_tests(&core, a, "Lisbon").await.unwrap();
        let arrival = scout_core::inbox::seed_arrival_for_tests(&core, a, "stay", "Hotel Alfama", "2026-10-12", Some(plan.trip.id)).await.unwrap();
        let (session, csrf) = signed_in(a);
        let mail = pending_mail_id(&app, &session).await;
        scout_core::inbox::seed_attachment_on_mail_for_tests(&core, mail, "ticket.pdf", "application/pdf", b"%PDF".to_vec()).await.unwrap();
        let res = post_json_with_cookie(&app, &format!("/chat/arrivals/{arrival}/add"), &session, Some(&csrf), r#"{}"#).await;
        assert_eq!(res.status(), StatusCode::OK);

        let res = get_with_cookie(&app, "/chat/trips", &session).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        let trip = body
            .as_array()
            .unwrap()
            .iter()
            .find(|trip| trip["name"] == "Lisbon")
            .expect("the trip the booking was added to");
        let item = trip["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["title"] == "Hotel Alfama")
            .expect("the item the booking became");
        assert_eq!(item["attachments"][0]["filename"], "ticket.pdf");
        assert_eq!(item["attachments"].as_array().unwrap().len(), 1);
        // And a leg nobody attached anything to says so with an empty list
        // rather than a missing key: the page reads `.length` on it.
        assert!(
            trip["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["attachments"].as_array().is_some_and(|a| a.is_empty())),
            "every item should carry the field, empty or not: {trip}",
        );
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
            "default-src 'none'; sandbox allow-downloads",
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
    async fn the_disposition_is_decided_from_the_sanitised_type_not_the_stored_one() {
        // A stored type in the spelling a mail client is entitled to send,
        // and which is not the one the allowlist names. If the handler ever
        // asks `disposition_for` about the raw string — the two lines are
        // adjacent and swapping them is a one-second edit — this ticket
        // silently stops opening, and nothing else in the suite notices.
        // `SafeMime` is what makes that swap fail to compile; this is what
        // makes it fail out loud if the type is ever loosened back.
        let (app, core, _dir) = inbox_app().await;
        let a = admitted(&core, "111").await;
        let id = scout_core::inbox::seed_attachment_for_tests(&core, a, "ticket.pdf", "APPLICATION/PDF", b"%PDF".to_vec()).await.unwrap();
        let (session, _) = signed_in(a);
        let res = get_with_cookie(&app, &format!("/chat/attachments/{id}"), &session).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["content-type"], "application/pdf");
        assert!(
            res.headers()["content-disposition"].to_str().unwrap().starts_with("inline;"),
            "decided from the stored type: {:?}",
            res.headers()["content-disposition"]
        );

        // And the same for the other half of the sanitising, a parameter:
        // a ticket that names itself in its type still opens.
        let named = scout_core::inbox::seed_attachment_for_tests(&core, a, "ticket.pdf", "application/pdf; name=\"ticket.pdf\"", b"%PDF".to_vec()).await.unwrap();
        let res = get_with_cookie(&app, &format!("/chat/attachments/{named}"), &session).await;
        assert_eq!(res.headers()["content-type"], "application/pdf");
        assert!(
            res.headers()["content-disposition"].to_str().unwrap().starts_with("inline;"),
            "{:?}",
            res.headers()["content-disposition"]
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
        // A response that downloads keeps the site's policy, from the
        // layer. Said out loud because nothing else pins the *narrowness*
        // of the strict one: putting `FILE_CSP` on every attachment
        // response would leave the rest of this suite green, and then the
        // day someone wants to know which responses carry which policy,
        // the answer would have quietly become "all of them".
        assert_eq!(res.headers()["content-security-policy"], crate::CSP);
    }

    #[tokio::test]
    async fn a_stored_page_is_neither_rendered_nor_offered_as_one() {
        // Belt and braces, and both halves pinned: `safe_mime` turns the
        // type into one no browser runs, and the disposition says save it.
        // Either alone would do; the point of asserting both is that
        // loosening one does not quietly become the only line of defence.
        let (app, core, _dir) = inbox_app().await;
        let a = admitted(&core, "111").await;
        // `text/html; charset=utf-8` rather than bare `text/html`: that is
        // how a mail client actually spells it, and it is the spelling the
        // parameter-stripping had to keep landing on `ACTIVE`.
        let id = scout_core::inbox::seed_attachment_for_tests(&core, a, "itinerary.html", "text/html; charset=utf-8", b"<script>1</script>".to_vec()).await.unwrap();
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
