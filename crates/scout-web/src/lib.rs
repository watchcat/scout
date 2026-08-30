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
mod email;
mod page;
mod pages;
mod ratelimit;
mod routes;
mod session;
mod telegram_login;

pub use cache::{refresh_forever, AdmissionCache, REFRESH};

use axum::extract::State;
use axum::http::header;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use scout_core::core::Core;
use std::sync::Arc;

/// Everything the signed-in half of the site needs. Absent when the
/// deployment has not been given the keys for it.
#[derive(Clone)]
pub struct AuthConfig {
    pub session_key: Vec<u8>,
    pub bot_token: String,
    pub resend_api_key: String,
    pub mail_from: String,
    pub base_url: String,
}

/// The router's state for the signed-in half. The limiters live here, in
/// one place, because they are shared across requests by definition — a
/// per-request limiter counts to one and stops nothing.
#[derive(Clone)]
pub struct AuthState {
    pub cfg: Arc<AuthConfig>,
    pub core: Arc<Core>,
    pub by_address: Arc<ratelimit::Limiter>,
    pub by_ip: Arc<ratelimit::Limiter>,
    pub mailer: email::Mailer,
}

impl AuthState {
    pub fn new(cfg: AuthConfig, core: Arc<Core>) -> Self {
        use std::time::Duration;
        let mailer = email::Mailer::Resend {
            api_key: cfg.resend_api_key.clone(),
            from: cfg.mail_from.clone(),
        };
        Self {
            cfg: Arc::new(cfg),
            core,
            mailer,
            by_address: Arc::new(ratelimit::Limiter::new(3, Duration::from_secs(900))),
            by_ip: Arc::new(ratelimit::Limiter::new(10, Duration::from_secs(3600))),
        }
    }
}

impl AuthConfig {
    /// Reads the environment, returning `None` unless every key is present.
    ///
    /// All or nothing on purpose: a half-configured deployment that serves
    /// a sign-in form and then cannot mail anything is worse than one that
    /// does not offer sign-in at all.
    pub fn from_env() -> Option<Self> {
        let key = std::env::var("SCOUT_SESSION_KEY").ok()?;
        if key.len() < 32 {
            tracing::warn!("SCOUT_SESSION_KEY is shorter than 32 bytes; sign-in stays off");
            return None;
        }
        Some(Self {
            session_key: key.into_bytes(),
            bot_token: std::env::var("TELEGRAM_BOT_TOKEN").ok()?,
            resend_api_key: std::env::var("RESEND_API_KEY").ok()?,
            mail_from: std::env::var("SCOUT_MAIL_FROM").ok()?,
            base_url: std::env::var("SCOUT_BASE_URL").ok()?,
        })
    }
}

/// The public page always; the signed-in half only when there are keys for
/// it.
///
/// The two halves are separate routers merged together rather than one
/// router with one state, because they hold different things: the public
/// page needs a cached admission and nothing else, and giving it a `Core`
/// it does not use would be an invitation to query the database from the
/// one path that exists to avoid doing that.
fn router(cache: AdmissionCache, auth: Option<AuthState>) -> Router {
    let public = Router::new()
        .route("/", get(index))
        // Liveness only. Deliberately says nothing about the database: a
        // health check that fails when DuckDB is busy would take the site
        // down for a reason the site does not have.
        .route("/healthz", get(|| async { "ok" }))
        .route("/icon.svg", get(icon))
        .with_state(cache);

    match auth {
        Some(auth) => public.merge(routes::auth::routes(auth)),
        None => public,
    }
}

async fn index(State(cache): State<AdmissionCache>) -> Html<String> {
    Html(page::render(&cache.get()))
}

/// The mark, as the browser tab's icon.
///
/// Served rather than inlined as a `data:` URI so it is fetched once and
/// cached, and so the header's copy of the glyph and the tab's icon can
/// differ: the tab needs the tile behind it to be legible against whatever
/// colour the browser paints, and the page does not.
async fn icon() -> impl IntoResponse {
    const ICON: &str = include_str!("icon.svg");
    ([(header::CONTENT_TYPE, "image/svg+xml"),
      (header::CACHE_CONTROL, "public, max-age=86400")], ICON)
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

    // Said out loud at start-up rather than left to be discovered by
    // probing: "the sign-in page 404s" is a symptom with several causes,
    // and this line names the one it actually is.
    let auth = AuthConfig::from_env().map(|cfg| AuthState::new(cfg, core.clone()));
    match &auth {
        Some(_) => tracing::info!("sign-in is configured"),
        None => tracing::info!("sign-in is not configured; serving the public page only"),
    }

    // Bind before spawning the refresher. The other order leaves a task
    // querying a single-writer database every thirty seconds, forever, for
    // a cache no reader will ever consult, on the one path where nobody is
    // reading: a bind that failed.
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tokio::spawn(refresh_forever(core, cache.clone()));

    tracing::info!(bind, "the front door is open");
    axum::serve(listener, router(cache, auth))
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
    use axum::response::Response;
    use tower::ServiceExt;

    pub(crate) const TEST_KEY: &[u8] = b"a test session key of at least 32 bytes";

    /// A router over a real Core on a throwaway database.
    ///
    /// A real Core rather than a fake because what these tests are for is
    /// the join between HTTP and the store — a fake would agree with
    /// whatever the handler believes about it.
    ///
    /// The TempDir is returned and must be held: dropping it deletes the
    /// database out from under the still-open connection.
    pub(crate) async fn test_app()
        -> (axum::Router, std::sync::Arc<scout_core::core::Core>, tempfile::TempDir)
    {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("test.duckdb");
        // Not `Config::for_test`: that is `#[cfg(test)]`, which means it
        // exists while scout-core tests itself and not from here.
        // `from_lookup` is the door it goes through anyway. The cost of
        // spelling it out is that a new required variable breaks this
        // line — loudly, in one place, which is the right way round.
        let cfg = scout_core::config::Config::from_lookup(|k| match k {
            "TELEGRAM_BOT_TOKEN" => Some("123456:test-bot-token".to_string()),
            "ALLOWED_TELEGRAM_USER_IDS" => Some("111".to_string()),
            "MINIMAX_API_KEY" => Some("mk".to_string()),
            "KAGI_API_KEY" => Some("kk".to_string()),
            "SCOUT_DB_PATH" => Some(db.to_str().unwrap().to_string()),
            _ => None,
        })
        .expect("the required variables are all set");
        let core = std::sync::Arc::new(scout_core::core::Core::start(cfg, None).unwrap());

        let auth = crate::AuthConfig {
            session_key: TEST_KEY.to_vec(),
            bot_token: "123456:test-bot-token".to_string(),
            resend_api_key: "test-key".to_string(),
            mail_from: "Scout <hello@example.com>".to_string(),
            base_url: "https://example.com".to_string(),
        };
        let cache = crate::cache::AdmissionCache::new(scout_core::core::Admission::Full);
        // Discard rather than Resend: with the real mailer the sign-in
        // tests fire an HTTPS request at api.resend.com, which makes the
        // suite depend on someone else's uptime and hands a test address
        // to a third party. Nothing else in this repository's tests binds
        // a socket or reaches the network.
        let mut state = crate::AuthState::new(auth, core.clone());
        state.mailer = crate::email::Mailer::Discard;
        let app = crate::router(cache, Some(state));
        (app, core, dir)
    }

    pub(crate) fn hash(token: &str) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
    }

    pub(crate) async fn issue(core: &scout_core::core::Core, token: &str) {
        scout_core::identity::issue_token(core, &hash(token), "a@example.com", None, 900)
            .await.unwrap();
    }

    pub(crate) async fn get(app: &axum::Router, uri: &str) -> Response {
        app.clone().oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await.unwrap()
    }

    pub(crate) async fn post_form(app: &axum::Router, uri: &str, form: &str) -> Response {
        app.clone().oneshot(
            Request::builder().method("POST").uri(uri)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(form.to_string())).unwrap()
        ).await.unwrap()
    }

    pub(crate) async fn body_of(res: Response) -> String {
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    }

    #[tokio::test]
    async fn the_root_serves_the_page_and_an_unknown_path_does_not() {
        let cache = crate::cache::AdmissionCache::new(scout_core::core::Admission::Full);
        let app = router(cache, None);

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

        // The page's <link rel="icon"> points here. A 404 costs nothing
        // visible — the tab just falls back to a blank page glyph — which
        // is exactly why it would go unnoticed.
        let icon = app.clone()
            .oneshot(Request::builder().uri("/icon.svg").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(icon.status(), StatusCode::OK);
        assert_eq!(icon.headers()["content-type"], "image/svg+xml");

        let missing = app
            .oneshot(Request::builder().uri("/wp-login.php").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn without_a_session_key_the_auth_routes_do_not_exist() {
        let cache = crate::cache::AdmissionCache::new(scout_core::core::Admission::Full);
        // Unconfigured: no key, so nothing that mints a session is served.
        // Booting with a generated default would sign sessions that a
        // restart could not verify, and nobody would notice until someone
        // forged one.
        let app = router(cache, None);
        let res = app
            .oneshot(Request::builder().uri("/sign-in").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
}
