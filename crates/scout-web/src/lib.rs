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
        // One HTTP client for the process, not one per message. See
        // `email::client`.
        let mailer = email::Mailer::Resend {
            api_key: cfg.resend_api_key.clone(),
            from: cfg.mail_from.clone(),
            http: email::client(),
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
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// The half of `from_env` that does not read the process environment,
    /// so it can be tested without one. `Config::from_lookup` is split the
    /// same way, for the same reason.
    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        // An empty variable is not a configured one. `env::var` hands back
        // `Ok("")` for `X=`, which is what a compose file with a blank
        // value or a Kubernetes secret missing a key produces — so four of
        // these five keys used to pass while empty, which is exactly the
        // half-configured deployment the all-or-nothing rule exists to
        // prevent. `Config::from_lookup` has always filtered this way; the
        // two disagreeing was the bug.
        let set = |k: &str| get(k).filter(|v| !v.trim().is_empty());

        let key = set("SCOUT_SESSION_KEY")?;
        if key.len() < 32 {
            tracing::warn!("SCOUT_SESSION_KEY is shorter than 32 bytes; sign-in stays off");
            return None;
        }
        Some(Self {
            session_key: key.into_bytes(),
            bot_token: set("TELEGRAM_BOT_TOKEN")?,
            resend_api_key: set("RESEND_API_KEY")?,
            mail_from: set("SCOUT_MAIL_FROM")?,
            base_url: set("SCOUT_BASE_URL")?,
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
        .with_state(Public { cache, sign_in: auth.is_some() });

    match auth {
        // The headers go on here rather than on the whole site because
        // this is the half that renders forms, sets cookies and embeds
        // somebody else's script. The public page has no script tag, no
        // input and nothing to steal, and a policy it does not need is a
        // policy that gets loosened for a reason that was never about it.
        Some(auth) => public.merge(
            routes::auth::routes(auth.clone())
                .merge(routes::account::routes(auth))
                .layer(axum::middleware::from_fn(security_headers)),
        ),
        None => public,
    }
    // HSTS goes on everything, unlike the headers above. It is a statement
    // about the host rather than about a page: a visitor who only ever
    // reads the landing page must still come away knowing never to try
    // this domain over HTTP, or the one request that matters — the first
    // one, before any redirect — stays interceptable. The Caddyfile set
    // this and the k3s ingress that replaced it does not, so the site has
    // been running without it; serving it from here means it does not
    // depend on which proxy is in front.
    .layer(axum::middleware::from_fn(hsts))
}

/// Tells the browser never to speak to this host over plain HTTP.
///
/// A year, with subdomains, matching what the Caddyfile asserted before the
/// move to k3s. `includeSubDomains` is safe here because nothing under this
/// domain serves HTTP at all — `send.goodscout.fyi` exists only as MX and
/// TXT records for mail.
///
/// Worth knowing that this is close to irreversible: a browser that has
/// seen it refuses plain HTTP for a year regardless of what is served
/// later, so shortening it does not take effect until each visitor comes
/// back.
async fn hsts(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::STRICT_TRANSPORT_SECURITY,
        axum::http::HeaderValue::from_static("max-age=31536000; includeSubDomains"),
    );
    response
}

/// What the public page needs: the cached admission, and whether there is
/// anywhere to sign in.
///
/// The second is a fact about the deployment, fixed at start-up, not
/// something that changes per request — but it has to reach `render`
/// somehow, and threading it through the one state the public routes
/// already share beats a second extractor or a global.
#[derive(Clone)]
struct Public {
    cache: AdmissionCache,
    sign_in: bool,
}

async fn index(State(public): State<Public>) -> Html<String> {
    Html(page::render(&public.cache.get(), public.sign_in))
}

/// What the browser may load, and what it may tell others about us.
///
/// `script-src` names `telegram.org` because the login widget is served
/// from there, and `frame-src` names `oauth.telegram.org` because that is
/// where the button the script draws actually lives —
/// `telegram-widget.js` builds its iframe as `widgetsOrigin + '/embed/'`,
/// and `widgetsOrigin` is `https://oauth.telegram.org`. A CSP host source
/// matches one host exactly, so naming only `telegram.org` would allow the
/// script and then blank the button it draws. Two hosts, one external
/// party, and the test below asserts that set rather than counting them:
/// anyone else's origin has to be added on this line, with a reason.
///
/// `frame-ancestors` is not here because `X-Frame-Options` says the same
/// thing and is understood by more of what sits in front of us.
const CSP: &str = "default-src 'self'; \
script-src 'self' https://telegram.org; \
frame-src https://oauth.telegram.org; \
img-src 'self' data:; \
style-src 'self' 'unsafe-inline'";

/// Puts the five headers on every response the signed-in half makes.
///
/// A layer rather than five tuples repeated in nine handlers, because the
/// handler that forgot them would be the one that mattered.
async fn security_headers(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::HeaderValue;
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    // Sign-in URLs carry tokens, so the path must never leave this site.
    // `strict-origin` sends `https://goodscout.fyi/` and never the path or
    // query, which keeps the token out of other people's logs just as
    // `no-referrer` did.
    //
    // It is `strict-origin` rather than `no-referrer` because of a second
    // effect that is easy to miss: per Fetch, a page served with
    // `no-referrer` makes the browser send `Origin: null` on form
    // submissions — the rule covers requests that are neither GET nor HEAD
    // and not in `cors` mode, which is every form here. Our own posts
    // therefore arrived naming nobody, `from_our_own_pages` had to allow
    // that case, and an attacker who set `no-referrer` on their own page
    // looked identical to us. Under `strict-origin` our forms carry a real
    // `Origin` and theirs carries theirs, so the check can refuse the
    // nameless request instead of waving it through.
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("strict-origin"));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    // Nothing this half serves is worth keeping a copy of. `/account`
    // renders whether you hold a seat and a form token that is live for
    // fifteen minutes, and `/auth/email` renders a page whose own URL
    // carries a login token — so a cached copy is a credential sitting in
    // a shared browser's back button or in an intermediary that decided
    // the response looked static. `no-store` rather than `no-cache`:
    // `no-cache` permits storing it and only requires revalidation, which
    // is the wrong half of the promise.
    //
    // On the whole half rather than on the two pages that need it, for the
    // reason the layer exists at all — and nothing here is worth caching
    // anyway. `/icon.svg` is on the public router and keeps its day.
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
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
        // `None`, like a deployment whose `getMe` failed: no bot name, so
        // no login widget and no Telegram link. The tests that need one
        // ask for it, so that the harness everything else shares does not
        // quietly assert a fact about Telegram.
        build_app(None, crate::email::Mailer::Discard).await
    }

    /// `test_app`, with the bot's own address that `getMe` would have
    /// returned.
    pub(crate) async fn test_app_named(
        return_url: Option<&str>,
    ) -> (axum::Router, std::sync::Arc<scout_core::core::Core>, tempfile::TempDir) {
        build_app(return_url, crate::email::Mailer::Discard).await
    }

    /// Every link the app would have mailed, in order.
    pub(crate) type Sent = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    /// `test_app`, keeping the mail instead of throwing it away.
    ///
    /// For the one thing that cannot be seen from outside: whether the
    /// token behind a link was filed against an account. The link is the
    /// only place that token ever appears, so a test that wants to spend
    /// one has to read the message.
    pub(crate) async fn test_app_keeping_mail()
        -> (axum::Router, std::sync::Arc<scout_core::core::Core>, tempfile::TempDir, Sent)
    {
        let sent: Sent = Default::default();
        let (app, core, dir) = build_app(None, crate::email::Mailer::Kept(sent.clone())).await;
        (app, core, dir, sent)
    }

    /// The link that was mailed, once the task that mails it has run.
    ///
    /// `mail_a_link` spawns, so the response comes back before the message
    /// does. Yielding until it appears rather than sleeping: on a
    /// current-thread runtime the spawned task runs at the next yield, and
    /// a fixed sleep would be both slower and still a guess.
    pub(crate) async fn mailed_link(sent: &Sent) -> String {
        for _ in 0..1000 {
            let first = sent.lock().unwrap().first().cloned();
            if let Some(link) = first {
                return link;
            }
            tokio::task::yield_now().await;
        }
        panic!("nothing was mailed");
    }

    async fn build_app(
        return_url: Option<&str>,
        mailer: crate::email::Mailer,
    ) -> (axum::Router, std::sync::Arc<scout_core::core::Core>, tempfile::TempDir) {
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
        let core = std::sync::Arc::new(
            scout_core::core::Core::start(cfg, return_url.map(str::to_string)).unwrap(),
        );

        let auth = crate::AuthConfig {
            session_key: TEST_KEY.to_vec(),
            bot_token: "123456:test-bot-token".to_string(),
            resend_api_key: "test-key".to_string(),
            mail_from: "Scout <hello@example.com>".to_string(),
            base_url: "https://example.com".to_string(),
        };
        let cache = crate::cache::AdmissionCache::new(scout_core::core::Admission::Full);
        // Never Resend: with the real mailer the sign-in tests fire an
        // HTTPS request at api.resend.com, which makes the suite depend on
        // someone else's uptime and hands a test address to a third party.
        // Nothing else in this repository's tests binds a socket or
        // reaches the network.
        let mut state = crate::AuthState::new(auth, core.clone());
        state.mailer = mailer;
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
                // A browser sets this on every form submission, and under
                // `strict-origin` it is a real origin rather than `null`.
                // Omitting it here would test a request no browser sends.
                .header("origin", "https://example.com")
                .body(Body::from(form.to_string())).unwrap()
        ).await.unwrap()
    }

    /// A `POST` carrying headers a browser would have set — `Origin` on a
    /// form submission, `Referer` when the page's referrer policy allows
    /// one. Sent as real headers rather than handed to the handler, so
    /// whatever reads them is on the path these tests exercise.
    pub(crate) async fn post_with_headers(
        app: &axum::Router,
        uri: &str,
        form: &str,
        extra: &[(&str, &str)],
    ) -> Response {
        let mut req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded");
        for (name, value) in extra {
            req = req.header(*name, *value);
        }
        app.clone().oneshot(req.body(Body::from(form.to_string())).unwrap()).await.unwrap()
    }

    /// The same `GET`, carrying a session cookie.
    ///
    /// Sent as a real `Cookie:` header rather than by handing the handler
    /// an account id, so `read_cookie` and `verify` are on the path these
    /// tests exercise — a handler that stopped reading the cookie would
    /// otherwise still pass them.
    pub(crate) async fn get_with_cookie(
        app: &axum::Router,
        uri: &str,
        session: &str,
    ) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("cookie", format!("{}={session}", crate::session::COOKIE))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    pub(crate) async fn post_with_cookie(
        app: &axum::Router,
        uri: &str,
        session: &str,
        form: &str,
    ) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("origin", "https://example.com")
                    .header("cookie", format!("{}={session}", crate::session::COOKIE))
                    .body(Body::from(form.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    pub(crate) async fn open_round(core: &scout_core::core::Core, code: &str, capacity: i64) {
        assert!(
            scout_core::invites::open_round(core, code, capacity).await.unwrap(),
            "the round `{code}` was already open"
        );
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

    #[test]
    fn an_empty_variable_does_not_count_as_a_configured_one() {
        // `X=` in a compose file is a variable somebody meant to fill in
        // and did not. Taken as set, it mounts a sign-in page that mails
        // through Resend with no API key and signs cookies with a key
        // nobody chose — the half-configured deployment `from_env` exists
        // to refuse.
        let full = |k: &str| {
            Some(match k {
                // Long enough to clear the 32-byte floor.
                "SCOUT_SESSION_KEY" => "a session key of at least 32 bytes".to_string(),
                _ => "value".to_string(),
            })
        };
        assert!(AuthConfig::from_lookup(full).is_some(), "a full environment was refused");

        for blank in ["", "   ", "\n"] {
            for name in [
                "SCOUT_SESSION_KEY",
                "TELEGRAM_BOT_TOKEN",
                "RESEND_API_KEY",
                "SCOUT_MAIL_FROM",
                "SCOUT_BASE_URL",
            ] {
                let cfg = AuthConfig::from_lookup(|k: &str| {
                    if k == name { Some(blank.to_string()) } else { full(k) }
                });
                assert!(cfg.is_none(), "{name}={blank:?} counted as configured");
            }
        }
    }

    #[tokio::test]
    async fn every_response_tells_the_browser_to_stay_on_https() {
        // On the public page too, not only the signed-in half. Somebody who
        // reads the landing page and comes back tomorrow must already know
        // not to try http:// — otherwise the first request of the next
        // visit, the one before any redirect, is still interceptable.
        let (app, _core, _dir) = test_app().await;
        for path in ["/", "/healthz", "/sign-in"] {
            let res = get(&app, path).await;
            assert_eq!(
                res.headers()["strict-transport-security"],
                "max-age=31536000; includeSubDomains",
                "{path} did not assert HSTS"
            );
        }

        // And when sign-in is not configured at all, so the whole
        // signed-in half is absent along with its header layer.
        let cache = crate::cache::AdmissionCache::new(scout_core::core::Admission::Full);
        let bare = crate::router(cache, None);
        let res = bare
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(
            res.headers().contains_key("strict-transport-security"),
            "an unconfigured deployment served no HSTS"
        );
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
