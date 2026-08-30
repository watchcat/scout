//! The routes that exist only when the deployment has been given keys.
//!
//! Kept apart from `lib.rs` so that "what the public sees" and "what a
//! signed-in visitor sees" are two files rather than two halves of one, and
//! so the mount point in `router` is a single line that is either there or
//! is not.

pub mod account;
pub mod auth;

use crate::{pages, session, AuthState};
use axum::http::{header, HeaderMap, Method, StatusCode};
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

/// The `scheme://host[:port]` an absolute URL names, lower-cased.
///
/// Parsed by hand rather than with a URL crate: the whole of what is
/// needed is the part before the first `/` after the scheme, and an
/// `Origin` header is already exactly that shape, so one function
/// normalises both sides of the comparison.
///
/// `None` for anything that is not an absolute URL — which includes
/// `null`, the value a browser sends for an opaque origin. That is the
/// honest answer: it names no site, so it cannot be compared to ours.
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    if scheme.is_empty() || authority.is_empty() {
        return None;
    }
    Some(format!("{}://{}", scheme.to_lowercase(), authority.to_lowercase()))
}

/// Whether a state-changing request could have come from a page of ours.
///
/// The form token cannot answer this. It is minted by a `GET` that is
/// served to strangers, is good for fifteen minutes and is not one-time,
/// so anybody who wants a valid one has one — it proves that Scout exists,
/// not that the `POST` came from Scout. This is the check it was standing
/// in for, and both stay: neither is enough alone.
pub(crate) fn from_our_own_pages(auth: &AuthState, headers: &HeaderMap) -> bool {
    // Nothing to compare against. A `SCOUT_BASE_URL` that is not an
    // absolute URL breaks the mailed link too, so this is not a state the
    // site survives — but it must fail closed rather than becoming
    // "everybody is same-origin".
    let Some(ours) = origin_of(&auth.cfg.base_url) else {
        return false;
    };

    // `Origin` first, and `Referer` only when `Origin` named nobody. A
    // request that names somebody else in `Origin` is refused on that
    // alone: falling through to a `Referer` it also controls would let an
    // attacker overrule the stronger header with the weaker one.
    let claimed = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .and_then(origin_of)
        .or_else(|| {
            headers.get(header::REFERER).and_then(|v| v.to_str().ok()).and_then(origin_of)
        });

    match claimed {
        Some(theirs) => theirs == ours,
        // Neither header names a site. Allowed — and this is the one
        // decision here that costs something, so, plainly:
        //
        // `security_headers` puts `Referrer-Policy: no-referrer` on every
        // page of this half. That suppresses `Referer` outright, and per
        // Fetch it also makes the browser send `Origin: null` on a form
        // submission from such a page — the rule covers requests that are
        // neither `GET` nor `HEAD` and are not in `cors` mode, which is
        // every form on this site. So a request with nothing to go on is
        // what our *own* forms look like, not a suspicious one, and
        // refusing it would refuse every real sign-in on the live site.
        //
        // What that leaves standing: an attacker posting from an ordinary
        // page of their own is refused, because their page sends a real
        // `Origin`. An attacker who puts `no-referrer` on their own page
        // is not, because they then look exactly like us. Closing that
        // needs `Referrer-Policy` on this half to become `strict-origin`
        // — which still keeps the mailed token out of anybody else's logs,
        // the reason `no-referrer` was chosen, while leaving our own posts
        // a real `Origin` — after which this arm becomes `false`. That is
        // a change to the security headers and is recorded in the review
        // rather than made here.
        None => true,
    }
}

/// Refuses a state-changing request that came from somebody else's page.
///
/// A layer over the routers rather than a line inside each handler,
/// because the handler that forgot it would be the one that mattered —
/// the same reason `security_headers` is a layer.
pub(crate) async fn only_from_our_own_pages(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // The safe methods change nothing here. The one `GET` that did,
    // `/auth/telegram`, is protected by a token in its own URL instead: a
    // cross-site top-level navigation is where `Origin` is least likely to
    // be there at all.
    let safe = matches!(*request.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if !safe && !from_our_own_pages(&auth, request.headers()) {
        // The page a bad form token gets. One refusal for a forged `POST`
        // and for a tab left open over lunch, for the same reason: which
        // check turned you away is not something a visitor is told.
        return stale_form();
    }
    next.run(request).await
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
