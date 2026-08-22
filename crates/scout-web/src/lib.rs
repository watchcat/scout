//! Scout's public face: one page, and a liveness check.
//!
//! Runs inside the Telegram binary today because DuckDB is single-writer and
//! a second process cannot open the file. In W4 the same crate is spawned by
//! the core binary instead; nothing here changes when it moves.

// Narrow this back from the `pub mod` Task 3 needed. That was to stop
// dead-code warnings while nothing called into it — `pub(crate)` does not
// silence those, since dead-code analysis is about reachability. Now that
// `serve` calls it, `mod` is enough, and leaving it public would give the
// same items two paths in from outside.
mod cache;
mod page;

pub use cache::{refresh_forever, AdmissionCache, REFRESH};

use axum::extract::State;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use scout_core::core::Core;
use std::sync::Arc;

fn router(cache: AdmissionCache) -> Router {
    Router::new()
        .route("/", get(index))
        // Liveness only. Deliberately says nothing about the database: a
        // health check that fails when DuckDB is busy would take the site
        // down for a reason the site does not have.
        .route("/healthz", get(|| async { "ok" }))
        .with_state(cache)
}

async fn index(State(cache): State<AdmissionCache>) -> Html<String> {
    Html(page::render(&cache.get()))
}

/// Serves until the process ends.
///
/// Takes the first reading before binding, so the page is never briefly
/// wrong on start-up. A failed first reading is not fatal, though: by this
/// point `Core::start` has already opened the database, so a failure here is
/// a transient query error rather than an unreachable store — and refusing
/// to open the front door over one would contradict the policy every later
/// refresh follows. It starts at `Full`, which is the safe thing to be wrong
/// about, and corrects itself within `REFRESH`.
pub async fn serve(core: Arc<Core>, bind: &str) -> anyhow::Result<()> {
    let first = core.admission().await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "could not read admission at start-up; opening as full");
        scout_core::core::Admission::Full
    });
    let cache = AdmissionCache::new(first);
    tokio::spawn(refresh_forever(core, cache.clone()));

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(bind, "the front door is open");
    axum::serve(listener, router(cache)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn the_root_serves_the_page_and_an_unknown_path_does_not() {
        let cache = crate::cache::AdmissionCache::new(scout_core::core::Admission::Full);
        let app = router(cache);

        let res = app.clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["content-type"], "text/html; charset=utf-8");

        // Deleting Task 2's test would otherwise leave /healthz uncovered,
        // and it is the route the proxy depends on.
        let health = app.clone()
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(health.status(), StatusCode::OK);

        let missing = app
            .oneshot(Request::builder().uri("/wp-login.php").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }
}
