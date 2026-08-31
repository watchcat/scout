//! The chat page: the seat that pays for it, what was already said, and a
//! message going in.
//!
//! `POST /chat/reset` is a later task's.

use super::{see_other, signed_in_as, sorry};
use crate::{session, AuthState};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use scout_core::identity;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::StreamExt;

const TEMPLATE: &str = include_str!("../chat.html");
const CLIENT: &str = include_str!("../chat.js");
const CSRF_TOKEN: &str = "<!--CSRF-->";

pub fn routes(auth: AuthState) -> Router {
    Router::new()
        .route("/chat", get(chat))
        .route("/chat.js", get(client))
        .route("/chat/history", get(history))
        .route("/chat/messages", post(send_message))
        // On the router rather than threaded through individual handlers —
        // the same reason it is on `account::routes` — so a handler that
        // forgot to check it is not the one that matters.
        .layer(axum::middleware::from_fn_with_state(
            auth.clone(),
            super::only_from_our_own_pages,
        ))
        .with_state(auth)
}

/// The account this request may spend model calls as, or the redirect that
/// says why not.
///
/// Shared between `chat` and `history`: both refuse a signed-out visitor
/// and a queued one identically, and a page that gated one way while its
/// own history endpoint gated another would be a door with two locks that
/// disagree.
async fn admitted_account(auth: &AuthState, headers: &HeaderMap) -> Result<i64, Response> {
    let Some(account_id) = signed_in_as(auth, headers) else {
        return Err(see_other("/sign-in"));
    };
    let standing = match identity::standing(&auth.core, account_id).await {
        Ok(standing) => standing,
        Err(e) => {
            tracing::error!(error = %e, "could not read an account's standing");
            return Err(sorry());
        }
    };
    if !standing.member {
        // The chat costs real model calls, and a queued account has not
        // been admitted to spend them. `/account` already explains where
        // they stand, so it does the explaining.
        return Err(see_other("/account"));
    }
    Ok(account_id)
}

async fn chat(axum::extract::State(auth): axum::extract::State<AuthState>, headers: HeaderMap) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    let csrf = session::csrf_for(&auth.cfg.session_key, account_id);
    Html(TEMPLATE.replace(CSRF_TOKEN, &csrf)).into_response()
}

async fn client() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], CLIENT)
}

async fn history(axum::extract::State(auth): axum::extract::State<AuthState>, headers: HeaderMap) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    match scout_core::session::transcript(&auth.core, account_id).await {
        Ok(turns) => axum::Json(turns).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "could not read a transcript");
            sorry()
        }
    }
}

/// Whether the `X-Scout-Csrf` header carries a token good for this
/// account. The form token used elsewhere rides in a hidden field because
/// those pages post a plain HTML form; this one posts JSON, so the same
/// value travels as a header instead — `csrf_ok_for` does not care which.
fn csrf_header_ok(auth: &AuthState, headers: &HeaderMap, account_id: i64) -> bool {
    headers
        .get("x-scout-csrf")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|token| session::csrf_ok_for(&auth.cfg.session_key, token, account_id))
}

/// Where this account's reminders should be delivered.
///
/// The rule this is meant to implement is "prefer the account's existing
/// Telegram address, if it has one" — a browser is not itself a delivery
/// channel, so a reminder made mid-run has to land somewhere else. Reading
/// that address is not possible from here: `note_delivery` and
/// `note_address` on `scout_core::core::Core` write the `deliveries` table
/// but nothing public reads it back, and `Store::telegram_ids` — the one
/// reader close enough to answer this — is `pub(crate)` to scout_core, not
/// visible from scout-web. Rather than reach around that boundary or guess
/// at an address, this always answers "web": a reminder made from a web
/// run today is created but has no channel polling it, so it simply never
/// delivers, instead of being sent to a Telegram chat this code cannot
/// verify still belongs to the same person. Closing this gap needs a
/// public accessor added to scout_core.
fn reply_to_for(account_id: i64) -> scout_api::ReplyTo {
    scout_api::ReplyTo { channel: "web".to_string(), address: account_id.to_string() }
}

/// Why a stream stopped. `AgentEvent` has no "finished", so a stream that
/// merely ended would be ambiguous — a completed answer, a refused run and a
/// crash all look identical to a client, and two of the three would leave a
/// spinner up forever. Every stream ends with exactly one of these.
#[derive(serde::Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum End {
    Ok,
    Busy,
    Error { message: String },
}

/// One thing that goes down the wire, and which SSE event name it takes.
///
/// `AgentEvent` is serialised untouched under `event: agent`, so the browser
/// and Telegram consume the identical shape — the protocol run_agent's
/// caller already speaks, not a second one invented for the web.
enum Frame {
    Agent(scout_api::AgentEvent),
    End(End),
}

/// Turns a receiver of `Frame`s into the SSE response every `/chat/messages`
/// path returns through, however it got here — a normal run, a daily cap
/// refused before one started, or nothing but the final `end`.
fn sse_response(rx: tokio::sync::mpsc::UnboundedReceiver<Frame>) -> Response {
    let stream = UnboundedReceiverStream::new(rx).map(|frame| {
        let event = match frame {
            Frame::Agent(e) => Event::default().event("agent").json_data(&e),
            Frame::End(end) => Event::default().event("end").json_data(&end),
        };
        // `AgentEvent` and `End` are plain data with no unserialisable
        // field, so a failure here would be a bug in one of those types,
        // not in a caller's input — worth a loud panic, not a silent drop.
        Ok::<_, std::convert::Infallible>(event.expect("a frame always serialises"))
    });
    Sse::new(stream).into_response()
}

#[derive(serde::Deserialize)]
struct MessageIn {
    text: String,
}

async fn send_message(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<MessageIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }

    if let Some(sentence) = scout_core::session::over_daily_cap(&auth.core, account_id).await {
        let (frames, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = frames.send(Frame::End(End::Error { message: sentence }));
        return sse_response(rx);
    }

    let conversation_id =
        match scout_core::session::resolve_conversation(&auth.core, account_id, "direct", &body.text).await {
            Ok(id) => id,
            Err(e) => {
                tracing::error!(error = %e, "could not open a conversation");
                return sorry();
            }
        };

    let run = scout_api::RunContext {
        account_id,
        conversation_id,
        reply_to: reply_to_for(account_id),
    };
    let core = auth.core.clone();
    let text = body.text;

    let (agent_tx, agent_rx) = tokio::sync::mpsc::unbounded_channel();
    let (frames, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        // Forward events as they happen. `run_agent` drops its sink when it
        // returns, which ends this loop — so awaiting the pump is what makes
        // `end` last rather than racing the final tokens.
        let pump = {
            let frames = frames.clone();
            tokio::spawn(async move {
                let mut agent_rx = agent_rx;
                while let Some(event) = agent_rx.recv().await {
                    let _ = frames.send(Frame::Agent(event));
                }
            })
        };
        let outcome = scout_core::run::run_agent(&core, agent_tx, &run, &text).await;
        let _ = pump.await;
        let _ = frames.send(Frame::End(match outcome {
            Ok(scout_core::run::RunOutcome::Answered(_)) => End::Ok,
            Ok(scout_core::run::RunOutcome::Busy) => End::Busy,
            Err(e) => End::Error { message: scout_core::run::agent_error_message(&e).to_string() },
        }));
    });

    sse_response(rx)
}

#[cfg(test)]
mod tests {
    use crate::tests::*;
    use axum::http::StatusCode;

    const DAY: i64 = 86_400;

    /// `test_app`, with a round open so a sign-in can actually admit
    /// someone rather than queuing them.
    async fn test_app_with_a_round()
        -> (axum::Router, std::sync::Arc<scout_core::core::Core>, tempfile::TempDir)
    {
        let (app, core, dir) = test_app().await;
        open_round(&core, "autumn", 5).await;
        (app, core, dir)
    }

    /// Signs in a Telegram id against an open round and returns the
    /// account id, panicking if the round had no room — every test that
    /// calls this one wants a member, not a queued visitor.
    async fn admitted(core: &scout_core::core::Core, telegram_id: &str) -> i64 {
        let scout_core::identity::SignIn::In { account_id } =
            scout_core::identity::sign_in(core, "telegram", telegram_id).await.unwrap()
        else {
            panic!("the round has room, so this should have admitted");
        };
        account_id
    }

    /// Seeds a two-message exchange under the `"direct"` scope, the same
    /// scope `/chat` reads from.
    ///
    /// `scout_core::session::save_history` is `pub(crate)` to scout-core,
    /// and so is `Core::store()` — the module that holds `Store` is not
    /// `pub` at all, so there is no path from here to write a message
    /// directly. `scout_core::session::seed_exchange_for_tests` was added to close
    /// that gap: it is the one door through `Store` that this crate did
    /// not already have, kept narrow (an exchange, not a `Store` handle)
    /// so the privacy boundary the crate's top-level doc comment describes
    /// stays intact everywhere else.
    async fn seed_conversation(core: &scout_core::core::Core, account_id: i64, you: &str, scout: &str) {
        scout_core::session::seed_exchange_for_tests(core, account_id, "direct", you, scout).await.unwrap();
    }

    #[tokio::test]
    async fn a_signed_out_visitor_is_sent_to_sign_in() {
        let (app, _core, _dir) = test_app().await;
        let res = get(&app, "/chat").await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers()["location"], "/sign-in");
    }

    #[tokio::test]
    async fn someone_still_waiting_is_sent_to_the_page_that_says_so() {
        // The chat costs real model calls, and a queued account has not
        // been admitted to spend them. `/account` already explains where
        // they stand, so it does the explaining.
        let (app, core, _dir) = test_app().await;
        let scout_core::identity::SignIn::Queued { account_id } =
            scout_core::identity::sign_in(&core, "telegram", "777").await.unwrap()
        else {
            panic!("no round is open, so this should have queued");
        };
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);

        let res = get_with_cookie(&app, "/chat", &cookie).await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers()["location"], "/account");
    }

    #[tokio::test]
    async fn a_member_gets_a_page_that_loads_its_script_from_us() {
        // The CSP has no 'unsafe-inline' for scripts, so an inline block
        // would be refused by the browser and the server would never know.
        // This test is the thing that notices.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);

        let page = body_of(get_with_cookie(&app, "/chat", &cookie).await).await;
        assert!(page.contains(r#"src="/chat.js""#), "the page loads no client");
        assert!(!page.contains("<script>"), "an inline script would be refused by our own CSP");
    }

    #[tokio::test]
    async fn the_client_script_is_served_as_javascript() {
        let (app, _core, _dir) = test_app().await;
        let res = get(&app, "/chat.js").await;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            res.headers()[axum::http::header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/javascript"),
            "a module served as the wrong type is refused by the browser"
        );
    }

    #[tokio::test]
    async fn history_is_empty_for_someone_who_has_never_spoken_and_creates_nothing() {
        // A page visit must not write rows.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);

        let body = body_of(get_with_cookie(&app, "/chat/history", &cookie).await).await;
        assert_eq!(body, "[]");

        assert!(
            scout_core::session::transcript(&core, account_id).await.unwrap().is_empty(),
            "asking for history minted a conversation"
        );
    }

    #[tokio::test]
    async fn history_returns_what_was_said() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        seed_conversation(&core, account_id, "cheapest beans", "here are three").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);

        let body = body_of(get_with_cookie(&app, "/chat/history", &cookie).await).await;
        assert!(body.contains("cheapest beans"), "got: {body}");
        assert!(body.contains(r#""You""#), "the role is not on the wire: {body}");
    }

    /// A JSON `POST`, carrying a session cookie and — when given — the
    /// `X-Scout-Csrf` header a real page would attach from its `<meta>` tag.
    async fn post_json_with_cookie(
        app: &axum::Router,
        uri: &str,
        session: &str,
        csrf: Option<&str>,
        body: &str,
    ) -> axum::response::Response {
        post_json_from_origin_opt(app, uri, session, csrf, "https://example.com", body).await
    }

    /// The same JSON `POST`, but naming the `Origin` a caller wants sent —
    /// so a test can exercise `only_from_our_own_pages` from outside it.
    async fn post_json_from_origin(
        app: &axum::Router,
        uri: &str,
        session: &str,
        csrf: &str,
        origin: &str,
        body: &str,
    ) -> axum::response::Response {
        post_json_from_origin_opt(app, uri, session, Some(csrf), origin, body).await
    }

    async fn post_json_from_origin_opt(
        app: &axum::Router,
        uri: &str,
        session: &str,
        csrf: Option<&str>,
        origin: &str,
        body: &str,
    ) -> axum::response::Response {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let mut req = axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .header("origin", origin)
            .header("cookie", format!("{}={session}", crate::session::COOKIE));
        if let Some(csrf) = csrf {
            req = req.header("x-scout-csrf", csrf);
        }
        let req: Request<Body> = req.body(Body::from(body.to_string())).unwrap();
        app.clone().oneshot(req).await.unwrap()
    }

    #[tokio::test]
    async fn a_post_without_the_csrf_header_is_refused() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let res = post_json_with_cookie(&app, "/chat/messages", &cookie, None, r#"{"text":"hi"}"#).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_post_from_another_origin_is_refused_even_with_a_good_token() {
        // The token proves Scout exists, not that the request came from
        // Scout. `only_from_our_own_pages` is the check that does.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);
        let res = post_json_from_origin(
            &app, "/chat/messages", &cookie, &csrf, "https://evil.example", r#"{"text":"hi"}"#,
        ).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn every_stream_ends_with_exactly_one_end_frame() {
        // Without it a client cannot tell a finished answer from a refusal
        // or a crash, and leaves a spinner up forever on two of the three.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);
        let body = body_of(
            post_json_with_cookie(&app, "/chat/messages", &cookie, Some(&csrf), r#"{"text":"hi"}"#).await,
        ).await;
        assert_eq!(body.matches("event: end").count(), 1, "got: {body}");
    }
}
