//! The routes that exist only when the deployment has been given keys.
//!
//! Kept apart from `lib.rs` so that "what the public sees" and "what a
//! signed-in visitor sees" are two files rather than two halves of one, and
//! so the mount point in `router` is a single line that is either there or
//! is not.

pub mod account;
pub mod auth;

use crate::{pages, session, AuthState};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};

/// The account this request proves, or `None`.
///
/// Here rather than in `session.rs` because this is where the two halves
/// meet: `session.rs` is arithmetic over a string and knows nothing about
/// HTTP, and both route modules need the same three steps — find the
/// header, find our cookie in it, check the signature. `None` covers no
/// cookie, a cookie for something else, a forged one and an expired one
/// alike, which is what leaves "signed out" and "tampered with" looking
/// the same from outside.
pub(crate) fn signed_in_as(auth: &AuthState, headers: &HeaderMap) -> Option<i64> {
    let jar = headers.get(header::COOKIE)?.to_str().ok()?;
    let value = session::read_cookie(jar, session::COOKIE)?;
    session::verify(&auth.cfg.session_key, &value)
}

pub(crate) fn see_other(to: &str) -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, to.to_string())]).into_response()
}

/// A form token that was never ours, has expired under a page left open,
/// or was minted for somebody else's session.
pub(crate) fn stale_form() -> Response {
    (StatusCode::BAD_REQUEST, Html(pages::stale_form())).into_response()
}

pub(crate) fn sorry() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, Html(pages::sorry())).into_response()
}
