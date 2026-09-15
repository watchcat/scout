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
use scout_core::inbox::{self, AddTarget, Outcome};

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
    h: String,
}

/// The two answers a live check gives. Two structs rather than one with
/// options so the free answer carries no `reason: null` and the taken one
/// no `handle`: the page reads `ok` and then exactly one other field.
#[derive(serde::Serialize)]
struct Free {
    ok: bool,
    handle: String,
}

#[derive(serde::Serialize)]
struct Refused {
    ok: bool,
    reason: String,
}

/// Whether `h` could be claimed right now, for a form that asks as the
/// person types.
///
/// A `200` either way: "taken" is an answer, not a failure, and a page
/// that had to tell a 4xx apart from a network fault to draw a red line
/// would draw it wrong. Rate-limited because each check is a database
/// read a stranger's script could drive at typing speed, and because it
/// is the one route that tells you whether a name is somebody's.
async fn check_handle(
    State(auth): State<AuthState>,
    headers: HeaderMap,
    Query(q): Query<CheckIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    // Its own bucket on the shared limiter, keyed like the title
    // suggestion's, so a burst of checks and a burst of titles do not
    // spend each other's allowance.
    if !auth.by_account.allow(&format!("handle:{account_id}")) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    match inbox::check_handle(&auth.core, &q.h).await {
        // `check_handle` said it is free, so normalising cannot fail now;
        // the fallback is only what the type asks for.
        Ok(Ok(())) => Json(Free {
            ok: true,
            handle: inbox::normalise_handle(&q.h).unwrap_or_default(),
        })
        .into_response(),
        Ok(Err(reason)) => Json(Refused { ok: false, reason }).into_response(),
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

/// Claims the address.
///
/// The rules are checked here before the store is asked, so that the
/// door's one inner `Err` left — "that one is taken" — can be a `409`
/// without this route matching on the sentence. A rule broken is `422`
/// with the sentence, which is what the live check already showed.
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
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Err(reason) = inbox::normalise_handle(&body.handle) {
        return (StatusCode::UNPROCESSABLE_ENTITY, Json(Refused { ok: false, reason })).into_response();
    }
    match inbox::set_handle(&auth.core, account_id, &body.handle).await {
        Ok(Ok(handle)) => Json(HandleOut { handle }).into_response(),
        Ok(Err(reason)) => (StatusCode::CONFLICT, Json(Refused { ok: false, reason })).into_response(),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not set a handle");
            sorry()
        }
    }
}

/// Where the person asked an arrival to go, as the page spells it: a
/// trip's id, the word `new`, or nothing for the trip the reading matched.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum TripPick {
    Id(i64),
    Word(String),
}

#[derive(serde::Deserialize)]
struct AddIn {
    #[serde(default)]
    trip: Option<TripPick>,
}

impl AddIn {
    /// `None` for a word that is not `new`: the only strings this field
    /// takes are the one keyword, and a client sending a trip's *name*
    /// here would otherwise be read as asking for a draft.
    fn target(&self) -> Option<AddTarget> {
        match &self.trip {
            None => Some(AddTarget::Matched),
            Some(TripPick::Id(id)) => Some(AddTarget::Trip(*id)),
            Some(TripPick::Word(w)) if w == "new" => Some(AddTarget::New),
            Some(TripPick::Word(_)) => None,
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
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(target) = body.target() else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(Refused { ok: false, reason: "trip is an id or \"new\"".into() }),
        )
            .into_response();
    };
    match inbox::add_arrival(&auth.core, account_id, arrival_id, target).await {
        Ok(Outcome::Done(plan)) => Json(plan).into_response(),
        Ok(Outcome::NotFound) => StatusCode::NOT_FOUND.into_response(),
        Ok(Outcome::NotPending) => StatusCode::CONFLICT.into_response(),
        Err(e) => {
            tracing::error!(error = %e, account_id, arrival_id, "could not add an arrival");
            sorry()
        }
    }
}

/// Puts the arrival under Other mail. `{}` rather than `204`: the page's
/// one fetch helper parses every answer, and an empty body is the one
/// shape it would have to special-case.
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
        return StatusCode::BAD_REQUEST.into_response();
    }
    match inbox::ignore_arrival(&auth.core, account_id, arrival_id).await {
        Ok(Outcome::Done(())) => Json(serde_json::json!({})).into_response(),
        // Somebody else's arrival and one that never existed are the same
        // answer, as everywhere on the signed-in half: a 403 would confirm
        // the row exists.
        Ok(Outcome::NotFound) => StatusCode::NOT_FOUND.into_response(),
        Ok(Outcome::NotPending) => StatusCode::CONFLICT.into_response(),
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
/// characters a header cannot be broken out of with, and the type is
/// passed through only when it is shaped like one. `nosniff` and
/// `attachment` together are what keep a file that claims to be a PDF
/// from being run as a page under our origin.
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
                HeaderValue::from_str(&format!("attachment; filename=\"{}\"", safe_filename(&filename)))
                    .expect("a sanitised filename is header-safe"),
            );
            h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
            // Said here as well as by the signed-half layer: a route that
            // serves somebody's ticket must not depend on where it is
            // mounted to stay out of a shared cache.
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, account_id, id, "could not read an attachment");
            sorry()
        }
    }
}

/// `[A-Za-z0-9._-]` of the name, anything else an underscore, never
/// empty. A stranger's filename is the one string on this route that
/// could carry a quote or a newline into a header.
fn safe_filename(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect();
    if safe.is_empty() {
        "file".to_string()
    } else {
        safe
    }
}

/// The stored type when it is shaped like one — `type/subtype` in the
/// characters a token allows — else the type that promises nothing.
fn safe_mime(mime: &str) -> String {
    let lower = mime.trim().to_ascii_lowercase();
    let token = |s: &str| {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'+' | b'-'))
    };
    match lower.split_once('/') {
        Some((kind, sub)) if token(kind) && token(sub) => lower,
        _ => "application/octet-stream".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{safe_filename, safe_mime};
    use crate::tests::*;
    use axum::http::StatusCode;

    #[test]
    fn a_strangers_filename_cannot_break_out_of_the_header() {
        assert_eq!(safe_filename("ticket.pdf"), "ticket.pdf");
        assert_eq!(safe_filename("my ticket\"; x=\r\ny.pdf"), "my_ticket___x___y.pdf");
        assert_eq!(safe_filename("билет.pdf"), "_____.pdf");
        assert_eq!(safe_filename(""), "file");
    }

    #[test]
    fn a_strangers_mime_is_passed_through_only_when_it_is_shaped_like_one() {
        assert_eq!(safe_mime("Application/PDF"), "application/pdf");
        assert_eq!(safe_mime("image/svg+xml"), "image/svg+xml");
        assert_eq!(safe_mime("text/html; charset=utf-8"), "application/octet-stream");
        assert_eq!(safe_mime("pdf"), "application/octet-stream");
        assert_eq!(safe_mime("a/b\r\nX: y"), "application/octet-stream");
    }

    #[tokio::test]
    async fn a_handle_is_chosen_once_checked_live_and_shown_with_the_domain() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let a = admitted(&core, "111").await;
        let (session, csrf) = signed_in(a);
        let res = get_with_cookie(&app, "/chat/handle/check?h=Sasha", &session).await;
        assert_eq!(body_of(res).await, r#"{"ok":true,"handle":"sasha"}"#);
        let res = post_json_with_cookie(&app, "/chat/handle", &session, Some(&csrf), r#"{"handle":"Sasha"}"#).await;
        assert_eq!(res.status(), StatusCode::OK);
        let res = get_with_cookie(&app, "/chat/inbox", &session).await;
        let v: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_eq!(v["handle"], "sasha");
        assert_eq!(v["domain"], "goodscout.fyi");
        let res = get_with_cookie(&app, "/chat/handle/check?h=postmaster", &session).await;
        assert!(body_of(res).await.contains("reserved"));
        let b = admitted(&core, "777").await;
        let (session_b, csrf_b) = signed_in(b);
        let res = post_json_with_cookie(&app, "/chat/handle", &session_b, Some(&csrf_b), r#"{"handle":"sasha"}"#).await;
        assert_eq!(res.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn an_arrival_is_added_to_its_trip_or_ignored_and_only_by_its_owner() {
        let (app, core, _dir) = test_app_with_a_round().await;
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
        let b = admitted(&core, "777").await;
        let other = scout_core::inbox::seed_arrival_for_tests(&core, a, "activity", "Museum", "2026-10-13", None).await.unwrap();
        let (session_b, csrf_b) = signed_in(b);
        let res = post_json_with_cookie(&app, &format!("/chat/arrivals/{other}/ignore"), &session_b, Some(&csrf_b), r#"{}"#).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let res = post_json_with_cookie(&app, &format!("/chat/arrivals/{other}/add"), &session, Some(&csrf), r#"{"trip":"new"}"#).await;
        assert_eq!(res.status(), StatusCode::OK, "a new draft is made and kept for it");
    }

    #[tokio::test]
    async fn an_attachment_is_served_to_its_owner_only() {
        let (app, core, _dir) = test_app_with_a_round().await;
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
}
