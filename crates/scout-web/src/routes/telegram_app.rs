//! The trip page as a Telegram Mini App.
//!
//! Not a second client. `/tg` is a launch page that trades the data
//! Telegram signed for an ordinary session on the same account the bot and
//! the website already share, and then sends the reader to `/chat` in its
//! Telegram shape — the same page, the same script, the same routes. What
//! lives here is only what Telegram changes: how the reader proves who
//! they are, who may frame the page, and how a file leaves it.

use crate::routes::chat::{admitted_account, csrf_header_ok, is_admitted};
use crate::routes::{inbox::attachment_response, trips::pdf_response};
use crate::{session, telegram_login, AuthState};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use scout_core::identity::{self, SignIn};

const LAUNCH_PAGE: &str = include_str!("../tg.html");
const LAUNCH_SCRIPT: &str = include_str!("../tg.js");
const TELEGRAM_SCRIPT: &str = include_str!("../telegram.js");

/// The site's policy with one change: Telegram Web may frame these pages.
///
/// Only Telegram Web, and only the pages that are the Mini App. The phone
/// and desktop apps open the page top-level and need nothing; the web
/// client puts it in an iframe on `web.telegram.org`, and every other
/// page on the site keeps `X-Frame-Options: DENY` — the shared layer
/// leaves it off only where a handler's policy names who may frame.
/// No `https://telegram.org` in `script-src` either, unlike the site's:
/// these pages do not load Telegram's script, see `telegram.js`.
pub(crate) const MINI_APP_CSP: &str = "default-src 'self'; \
script-src 'self'; \
img-src 'self' data:; \
style-src 'self' 'unsafe-inline'; \
frame-ancestors https://web.telegram.org";

/// What `initData` may weigh. Telegram's is a few hundred bytes; this is
/// a bound on what gets parsed, not a guess at its size.
const MAX_INIT_DATA: usize = 4096;

pub fn routes(auth: AuthState) -> Router {
    Router::new()
        .route("/tg", get(launch_page))
        .route("/tg.js", get(|| async { script(LAUNCH_SCRIPT) }))
        .route("/telegram.js", get(|| async { script(TELEGRAM_SCRIPT) }))
        .route("/tg/session", post(start_session))
        .route("/tg/file", get(open_file))
        .route("/chat/file-link", post(file_link))
        .with_state(auth)
}

fn script(body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

/// A response the Mini App may be framed around.
pub(crate) fn framable(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(MINI_APP_CSP));
    response
}

async fn launch_page(State(auth): State<AuthState>) -> Response {
    // Nobody is signed in yet, so the form token names nobody — the one the
    // sign-in page uses, for the one request this page makes.
    let page = LAUNCH_PAGE.replace("<!--CSRF-->", &session::csrf(&auth.cfg.session_key));
    framable(Html(page).into_response())
}

#[derive(serde::Deserialize)]
struct SessionIn {
    init_data: String,
}

/// Launch data in, session out.
///
/// The Telegram id is the whole of the claim, so a session already in the
/// jar does not change the answer: inside Telegram, Telegram says who this
/// is. Signing in may seat the account in an open round, exactly as the
/// Telegram button on the sign-in page does — the same call, so the two
/// ways in cannot disagree about who gets a seat.
///
/// Refused without a cookie when the account is not admitted: a queued
/// account holding a session would only be sent on to `/account`, which
/// is a page about the website, inside Telegram.
async fn start_session(State(auth): State<AuthState>, headers: HeaderMap, Json(body): Json<SessionIn>) -> Response {
    let token_ok = headers
        .get("x-scout-csrf")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| session::csrf_ok(&auth.cfg.session_key, t));
    if !token_ok || body.init_data.len() > MAX_INIT_DATA {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(telegram_id) = telegram_login::verify_web_app(&auth.cfg.bot_token, &body.init_data) else {
        // Said once, without the data: this is the line to look for when a
        // launch from a real client keeps failing, and the data is a
        // credential for the next hour.
        tracing::info!("a Mini App launch did not verify");
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let account_id = match identity::sign_in(&auth.core, "telegram", &telegram_id.to_string()).await {
        Ok(SignIn::In { account_id } | SignIn::Queued { account_id }) => account_id,
        Err(e) => {
            tracing::error!(error = %e, "could not sign in a Mini App launch");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    match is_admitted(&auth, account_id).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::FORBIDDEN.into_response(),
        Err(response) => return response,
    }
    let cookie = session::mint(&auth.cfg.session_key, account_id, session::SESSION_TTL_SECS);
    (
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, session::set_embedded_cookie(&cookie, session::SESSION_TTL_SECS))],
    )
        .into_response()
}

#[derive(serde::Deserialize)]
struct FileLinkIn {
    attachment: Option<i64>,
    trip: Option<String>,
}

/// A few minutes' link to one of this account's files, for a page whose
/// links open in a browser that holds no session.
///
/// Minted for whatever is named, without looking: whether the file is this
/// account's is decided when the link is opened, by the same check a
/// cookie gets, so a link to somebody else's ticket opens nothing.
async fn file_link(State(auth): State<AuthState>, headers: HeaderMap, Json(body): Json<FileLinkIn>) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let what = match (body.attachment, body.trip) {
        (Some(id), None) => format!("attachment:{id}"),
        (None, Some(trip)) if !trip.is_empty() => format!("trip:{trip}"),
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    let token = session::file_link(&auth.cfg.session_key, account_id, &what, session::FILE_LINK_TTL_SECS);
    let base = auth.cfg.base_url.trim_end_matches('/');
    Json(serde_json::json!({ "url": format!("{base}/tg/file?t={token}") })).into_response()
}

#[derive(serde::Deserialize)]
struct FileQuery {
    t: String,
}

/// Opens what a file link names. No cookie is read: the link is the proof,
/// and a browser outside Telegram has no cookie to offer anyway.
async fn open_file(State(auth): State<AuthState>, Query(query): Query<FileQuery>) -> Response {
    let Some((account_id, what)) = session::file_link_ok(&auth.cfg.session_key, &query.t) else {
        return (StatusCode::GONE, Html(EXPIRED)).into_response();
    };
    if let Some(id) = what.strip_prefix("attachment:").and_then(|id| id.parse().ok()) {
        return attachment_response(&auth, account_id, id).await;
    }
    if let Some(trip) = what.strip_prefix("trip:") {
        return pdf_response(&auth, account_id, trip).await;
    }
    (StatusCode::GONE, Html(EXPIRED)).into_response()
}

const EXPIRED: &str = "<!DOCTYPE html><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>Link expired — Scout</title>\
<body style=\"margin:0;padding:32px 20px;background:#002b36;color:#839496;font:16px/1.6 system-ui,sans-serif\">\
<h1 style=\"color:#eee8d5;font-size:20px;margin:0 0 8px\">This link has expired</h1>\
<p style=\"margin:0\">Links to tickets and trip PDFs work for five minutes. Open the file again from your trip in Telegram.</p></body>";

#[cfg(test)]
mod tests {
    use crate::tests::*;
    use axum::http::StatusCode;

    const BOT: &str = "123456:test-bot-token";

    fn launch_data(telegram_id: i64) -> String {
        let user = format!(r#"{{"id":{telegram_id},"first_name":"Sasha"}}"#);
        crate::telegram_login::web_app_data_like_telegram(
            BOT,
            &[("auth_date", &chrono::Utc::now().timestamp().to_string()), ("user", &user)],
        )
    }

    async fn start(app: &axum::Router, csrf: Option<&str>, init_data: &str) -> axum::response::Response {
        use tower::ServiceExt;
        let mut req = axum::http::Request::post("/tg/session")
            .header("content-type", "application/json")
            .header("origin", "https://example.com");
        if let Some(csrf) = csrf {
            req = req.header("x-scout-csrf", csrf);
        }
        let body = serde_json::json!({ "init_data": init_data }).to_string();
        app.clone().oneshot(req.body(axum::body::Body::from(body)).unwrap()).await.unwrap()
    }

    fn header<'a>(res: &'a axum::response::Response, name: &str) -> Option<&'a str> {
        res.headers().get(name).and_then(|v| v.to_str().ok())
    }

    #[tokio::test]
    async fn the_launch_page_may_be_framed_by_telegram_web_and_nobody_else() {
        let (app, _core, _dir) = test_app().await;
        let res = get(&app, "/tg").await;
        assert_eq!(res.status(), StatusCode::OK);
        let csp = header(&res, "content-security-policy").unwrap().to_string();
        assert!(csp.contains("frame-ancestors https://web.telegram.org"), "{csp}");
        assert!(!csp.contains("script-src 'self' https://telegram.org"), "Telegram's script is not loaded here");
        assert!(header(&res, "x-frame-options").is_none(), "DENY would blank the frame in a browser that reads it");
        let page = body_of(res).await;
        assert!(!page.contains("<!--CSRF-->"), "the form token was not filled in");

        // Everywhere else keeps the refusal.
        let sign_in = get(&app, "/sign-in").await;
        assert_eq!(header(&sign_in, "x-frame-options"), Some("DENY"));
    }

    #[tokio::test]
    async fn a_member_launching_from_telegram_gets_a_session_that_survives_a_frame() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let csrf = crate::session::csrf(TEST_KEY);

        let res = start(&app, Some(&csrf), &launch_data(777)).await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let cookie = header(&res, "set-cookie").unwrap().to_string();
        assert!(cookie.contains("SameSite=None") && cookie.contains("Partitioned"), "{cookie}");
        let value = cookie.split(';').next().unwrap().split_once('=').unwrap().1;
        assert_eq!(crate::session::verify(TEST_KEY, value), Some(account_id), "the bot's account, not a new one");
    }

    #[tokio::test]
    async fn a_launch_that_is_not_telegrams_or_not_from_our_page_starts_nothing() {
        let (app, core, _dir) = test_app_with_a_round().await;
        admitted(&core, "777").await;
        let csrf = crate::session::csrf(TEST_KEY);

        let forged = launch_data(777).replace("777", "778");
        let res = start(&app, Some(&csrf), &forged).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(header(&res, "set-cookie").is_none());

        let res = start(&app, None, &launch_data(777)).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(header(&res, "set-cookie").is_none());
    }

    #[tokio::test]
    async fn someone_without_a_seat_is_told_so_and_given_no_cookie() {
        // No round open: signing in resolves an account but cannot seat it.
        let (app, _core, _dir) = test_app().await;
        let res = start(&app, Some(&crate::session::csrf(TEST_KEY)), &launch_data(555)).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(header(&res, "set-cookie").is_none());
    }

    #[tokio::test]
    async fn the_trip_page_in_telegram_says_where_it_is_and_signed_out_goes_back_to_the_launch() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let (cookie, _csrf) = signed_in(account_id);

        let res = get_with_cookie(&app, "/chat?in=telegram", &cookie).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(header(&res, "content-security-policy").unwrap().contains("frame-ancestors https://web.telegram.org"));
        assert!(header(&res, "x-frame-options").is_none());
        assert!(body_of(res).await.contains(r#"<html lang="en" data-surface="telegram">"#));

        // The site's own chat page is unchanged.
        let res = get_with_cookie(&app, "/chat", &cookie).await;
        assert_eq!(header(&res, "x-frame-options"), Some("DENY"));
        assert!(body_of(res).await.contains(r#"<html lang="en">"#), "the site page is not told it is in Telegram");

        // Signed out inside Telegram: back to the launch page, trip kept.
        let res = get(&app, "/chat?in=telegram&trip=Hong%20Kong").await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(header(&res, "location"), Some("/tg?trip=Hong+Kong"));
        let res = get(&app, "/chat").await;
        assert_eq!(header(&res, "location"), Some("/sign-in"));
    }

    #[tokio::test]
    async fn a_file_link_opens_the_ticket_without_a_cookie_and_only_that_accounts() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let mine = admitted(&core, "777").await;
        let theirs = admitted(&core, "888").await;
        let ticket = scout_core::inbox::seed_attachment_for_tests(&core, mine, "eticket.pdf", "application/pdf", b"%PDF-mine".to_vec())
            .await
            .unwrap();
        let other = scout_core::inbox::seed_attachment_for_tests(&core, theirs, "other.pdf", "application/pdf", b"%PDF-theirs".to_vec())
            .await
            .unwrap();
        let (cookie, csrf) = signed_in(mine);

        let link = |id: i64| {
            let app = app.clone();
            let (cookie, csrf) = (cookie.clone(), csrf.clone());
            async move {
                let res = post_json_with_cookie(&app, "/chat/file-link", &cookie, Some(&csrf), &format!(r#"{{"attachment":{id}}}"#)).await;
                assert_eq!(res.status(), StatusCode::OK);
                let body: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
                body["url"].as_str().unwrap().strip_prefix("https://example.com").unwrap().to_string()
            }
        };

        let res = get(&app, &link(ticket).await).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_of(res).await, "%PDF-mine");

        // A link can be minted for anything and opens only what is yours.
        let res = get(&app, &link(other).await).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        let res = get(&app, "/tg/file?t=not-a-link").await;
        assert_eq!(res.status(), StatusCode::GONE);

        let res = post_json_with_cookie(&app, "/chat/file-link", &cookie, None, &format!(r#"{{"attachment":{ticket}}}"#)).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "no form token, no link");
    }
}
