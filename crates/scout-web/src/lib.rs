//! Scout's public face: one page, and a liveness check.
//!
//! Runs inside the Telegram binary today because DuckDB is single-writer and
//! a second process cannot open the file. In W4 the same crate is spawned by
//! the core binary instead; nothing here changes when it moves.

#[cfg(test)]
use axum::routing::get;
#[cfg(test)]
use axum::Router;

/// Liveness only. Deliberately says nothing about the database: a health
/// check that fails when DuckDB is busy would take the site down for a
/// reason the site does not have.
///
/// Test-only for now because nothing serves it yet: the binary gains a
/// listener in a later task, and this loses the `cfg` when it does. Marking
/// it as what it currently is beats an `allow(dead_code)` that would outlive
/// the reason for it.
#[cfg(test)]
fn health_router() -> Router {
    Router::new().route("/healthz", get(|| async { "ok" }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn the_proxy_can_tell_whether_we_are_alive() {
        // Driven through the router rather than a socket: no port to bind,
        // nothing to conflict with, and it runs in the same suite as
        // everything else.
        let app = health_router();
        let res = app
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
}
