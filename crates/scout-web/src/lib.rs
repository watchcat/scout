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

    // Bind before spawning the refresher. The other order leaves a task
    // querying a single-writer database every thirty seconds, forever, for
    // a cache no reader will ever consult, on the one path where nobody is
    // reading: a bind that failed.
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tokio::spawn(refresh_forever(core, cache.clone()));

    tracing::info!(bind, "the front door is open");
    axum::serve(listener, router(cache))
        .with_graceful_shutdown(closing_time())
        .await?;
    Ok(())
}

/// Resolves when the container is being replaced.
///
/// Without this the server accepts connections for the whole of the bot's
/// drain window — up to 330 seconds — answering `/healthz` with 200 while
/// the container is on its way out, and then cuts whatever is in flight the
/// moment the runtime drops. Stopping when the signal arrives means a
/// request either completes or was never accepted.
///
/// This listens for itself rather than sharing the bot's handler: two
/// listeners for one signal is what tokio's signal handling is for, and the
/// alternative is a channel threaded through two crates for the sake of one
/// bool.
async fn closing_time() {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            // Nothing to do but keep serving, which is the behaviour this
            // function replaced.
            tracing::warn!(error = %e, "cannot listen for SIGTERM; the page will serve until the process dies");
            return std::future::pending().await;
        }
    };
    term.recv().await;
    tracing::info!("the front door is closing");
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

        // Nothing consumes /healthz today — the proxy dials per request
        // rather than probing, deliberately. It stays because a container
        // wants a way to be asked whether it is alive that does not involve
        // rendering a page, and because W2 or an uptime check will want it.
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
