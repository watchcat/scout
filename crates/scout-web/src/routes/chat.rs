//! The chat page: the seat that pays for it, what was already said, a
//! message going in, and a fresh thread.

use super::{see_other, signed_in_as, sorry};
use crate::{session, AuthState};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
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
        .route("/chat/reset", post(reset))
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
    // Founder *or* member, the same pair the Telegram gate asks about. A
    // founder is admitted by `ALLOWED_TELEGRAM_USER_IDS` and deliberately
    // holds no `members` row — the account-keying work made a point of
    // handing back a seat one had picked up by accident — so a gate reading
    // only membership would turn away the people paying for the bot.
    let founder = match auth.core.founder_account(account_id).await {
        Ok(founder) => founder,
        Err(e) => {
            tracing::error!(error = %e, "could not tell whether an account is a founder");
            return Err(sorry());
        }
    };
    if !standing.member && !founder {
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

/// Where a reminder made during this run should be delivered, or `None`.
///
/// A browser is not a delivery channel: nothing polls it, so a reminder
/// recorded against one would simply never arrive. The account's Telegram
/// chat is the only place a web run can promise to come back to, and when
/// there is no Telegram identity there is nowhere at all — in which case
/// `build_agent` does not offer the reminder tool, and the model cannot
/// accept a promise the system would silently break.
async fn reply_to_for(auth: &AuthState, account_id: i64) -> Option<scout_api::ReplyTo> {
    match identity::delivery_address(&auth.core, account_id, "telegram").await {
        Ok(address) => address.map(|address| scout_api::ReplyTo {
            channel: "telegram".to_string(),
            address,
        }),
        Err(e) => {
            // Not fatal: the run is still worth doing, it just cannot take
            // on a reminder.
            tracing::warn!(error = %e, "could not read a delivery address");
            None
        }
    }
}

/// Why a stream stopped. `AgentEvent` has no "finished", so a stream that
/// merely ended would be ambiguous — a completed answer, a refused run and a
/// crash all look identical to a client, and two of the three would leave a
/// spinner up forever. Every stream ends with exactly one of these.
#[derive(serde::Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum End {
    Ok { answer: String },
    Busy,
    Error { message: String },
}

/// The last frame of a stream, given how the run ended.
///
/// A function rather than three arms inline, because the arm that matters
/// is invisible from a test otherwise: `run_agent` needs a live model, so
/// nothing in this workspace can drive `send_message` end to end.
///
/// Measured with this inline: `Answered(_)` threw the finished answer away
/// and the page kept whatever the token stream had left in the bubble.
/// That is not the answer. `streamed` accumulates every text delta of every
/// turn of a multi-turn run, so it holds all the "let me check the next
/// shop" narration the model writes between tool calls, and it predates
/// both the wrap-up rewrite and the dead-link repair — so the page could
/// also show links that `run_agent` had already removed for being dead.
/// Telegram has always used the returned reply; only the browser did not.
fn end_frame(outcome: anyhow::Result<scout_core::run::RunOutcome>) -> End {
    match outcome {
        Ok(scout_core::run::RunOutcome::Answered(answer)) => End::Ok { answer },
        Ok(scout_core::run::RunOutcome::Busy) => End::Busy,
        // Telegram has always logged this; the browser path did not, so a
        // failed run left no trace anywhere and the only record of it was
        // the reader's screenshot. `run_agent` logs the stream errors it
        // raises itself, but not every failure comes from there.
        Err(e) => {
            tracing::error!(error = %e, "a browser run failed");
            End::Error { message: scout_core::run::agent_error_message(&e).to_string() }
        }
    }
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
    // A run is silent for long stretches — a tool call takes most of a
    // minute, and after the last token the dead-link probes can hold the
    // connection for another twelve seconds without sending anything. With
    // no traffic at all, any intermediary with an idle timeout is free to
    // drop the stream, and the reader gets "the connection dropped" for a
    // run that was going perfectly well. The comment frames this sends are
    // ignored by `parseFrame`, which needs an `event:` line.
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
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
        reply_to: reply_to_for(&auth, account_id).await,
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
        let _ = frames.send(Frame::End(end_frame(outcome)));
    });

    sse_response(rx)
}

async fn reset(axum::extract::State(auth): axum::extract::State<AuthState>, headers: HeaderMap) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match scout_core::session::reset(&auth.core, account_id, "direct").await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "could not reset a conversation");
            sorry()
        }
    }
}

#[cfg(test)]
mod tests {
    // Named imports rather than `use super::*`: the module imports axum's
    // `get` and `post`, which would shadow the test helpers of the same
    // name that every request in here goes through.
    use super::{end_frame, reply_to_for, AuthState};
    use crate::tests::*;

    #[test]
    fn a_failed_run_is_written_down_before_it_is_apologised_for() {
        // Measured: a run died at turn 2 of 20 and the whole pod log held
        // no WARN and no ERROR, because this path turned the error into a
        // sentence for the browser and dropped it. The apology is what the
        // reader gets; the log is the only thing left to diagnose from.
        let src = include_str!("chat.rs");
        let start = src.find("fn end_frame").expect("the end frame must exist");
        let end = src[start..].find("\n}").expect("it must end") + start;
        assert!(
            src[start..end].contains("tracing::error!"),
            "a failed run must be logged, not only apologised for"
        );
    }

    #[test]
    fn the_stream_is_kept_alive_through_a_silent_run() {
        // Not assertable from the response type, so it is asserted from the
        // source. A run goes quiet for a whole tool call, and a stream that
        // sends nothing at all invites an intermediary to close it.
        let src = include_str!("chat.rs");
        let start = src.find("fn sse_response").expect("the response builder must exist");
        let end = src[start..].find("\n}").expect("it must end") + start;
        assert!(
            src[start..end].contains("keep_alive"),
            "a silent stream must still send something, or it gets dropped as idle"
        );
    }

    #[test]
    fn the_last_frame_carries_the_finished_answer() {
        // The bubble the reader is looking at was built from token deltas,
        // and those are every turn of the run concatenated. The finished
        // answer only exists in `run_agent`'s return value; if the last
        // frame does not carry it across, nothing else will and the page
        // keeps the narration.
        let end = end_frame(Ok(scout_core::run::RunOutcome::Answered(
            "EUR 10.99 at bol.com".to_string(),
        )));
        assert_eq!(
            serde_json::to_value(&end).unwrap(),
            serde_json::json!({"status": "ok", "answer": "EUR 10.99 at bol.com"})
        );
    }

    #[test]
    fn an_answer_that_was_all_reasoning_arrives_as_an_empty_one() {
        // Not a missing field: `strip_thinking` leaving nothing means there
        // was no answer in it, and the page has to be told to clear the
        // bubble rather than keep what it had. Absent and empty are
        // different instructions and the client reads them differently.
        let end = end_frame(Ok(scout_core::run::RunOutcome::Answered(String::new())));
        assert_eq!(
            serde_json::to_value(&end).unwrap(),
            serde_json::json!({"status": "ok", "answer": ""})
        );
    }

    #[test]
    fn a_busy_or_failed_run_claims_no_answer() {
        assert_eq!(
            serde_json::to_value(end_frame(Ok(scout_core::run::RunOutcome::Busy))).unwrap(),
            serde_json::json!({"status": "busy"})
        );
        let failed = serde_json::to_value(end_frame(Err(anyhow::anyhow!("boom")))).unwrap();
        assert_eq!(failed["status"], "error");
        assert!(
            failed.get("answer").is_none(),
            "a run that failed must not hand the page an answer to display"
        );
    }

    /// The state the handlers take, built the way `build_app` builds it —
    /// needed for the helpers that are tested directly rather than through
    /// a request.
    fn auth_state(core: &std::sync::Arc<scout_core::core::Core>) -> AuthState {
        AuthState::new(
            crate::AuthConfig {
                session_key: TEST_KEY.to_vec(),
                bot_token: "123456:test-bot-token".to_string(),
                resend_api_key: "test-key".to_string(),
                mail_from: "Scout <hello@example.com>".to_string(),
                base_url: "https://example.com".to_string(),
            },
            core.clone(),
        )
    }
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
    async fn a_run_promises_a_reminder_only_where_one_could_be_delivered() {
        // A browser is not a delivery channel. If this returned something
        // for an account with no Telegram, `build_agent` would offer the
        // reminder tool and the model would accept a promise that nothing
        // polls — the reminder would be written and never arrive.
        let (_app, core, _dir) = test_app_with_a_round().await;

        // Someone who signed in by email and never linked Telegram.
        let scout_core::identity::SignIn::In { account_id: web_only } =
            scout_core::identity::sign_in(&core, "email", "ada@example.com").await.unwrap()
        else {
            panic!("the round had room");
        };
        let auth = auth_state(&core);
        assert_eq!(
            reply_to_for(&auth, web_only).await,
            None,
            "a run with nowhere to deliver was handed a destination anyway"
        );

        // Someone whose Telegram chat Scout has actually seen.
        let telegram = admitted(&core, "777").await;
        core.note_address(777, "telegram", "12345".to_string()).await.unwrap();
        assert_eq!(
            reply_to_for(&auth, telegram).await,
            Some(scout_api::ReplyTo {
                channel: "telegram".to_string(),
                address: "12345".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn a_founder_gets_in_without_ever_holding_a_seat() {
        // `Config::for_test`-style config makes telegram 111 the founder.
        // No round is open here, so nobody can be seated at all — which is
        // the point: founders are admitted by the allow-list and hold no
        // members row, and the reconciliation added earlier hands back any
        // seat one picks up. A gate reading only membership locks out the
        // people who pay for the bot.
        let (app, core, _dir) = test_app().await;
        let (scout_core::identity::SignIn::In { account_id }
        | scout_core::identity::SignIn::Queued { account_id }) =
            scout_core::identity::sign_in(&core, "telegram", "111").await.unwrap();
        assert!(
            !scout_core::identity::standing(&core, account_id).await.unwrap().member,
            "this test is vacuous if the founder holds a seat"
        );
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);

        let res = get_with_cookie(&app, "/chat", &cookie).await;
        assert_eq!(res.status(), StatusCode::OK, "a founder was turned away from the chat");
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
    async fn the_page_still_carries_every_id_the_client_binds_to() {
        // Restyling is exactly the edit that silently unhooks a handler:
        // the JS asks for these by id, and a missing one fails in the
        // browser and nowhere else. Moving the reset control between
        // sections is the specific risk here.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let page = body_of(get_with_cookie(&app, "/chat", &cookie).await).await;

        for id in ["turns", "status", "notice", "ask", "text", "send", "reset"] {
            assert!(page.contains(&format!(r#"id="{id}""#)), "the client binds to #{id}: {page}");
        }
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

    #[tokio::test]
    async fn a_reset_starts_a_thread_that_does_not_remember_the_last_one() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        seed_conversation(&core, account_id, "cheapest beans", "here are three").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);

        let res = post_json_with_cookie(&app, "/chat/reset", &cookie, Some(&csrf), "").await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        assert!(
            scout_core::session::transcript(&core, account_id).await.unwrap().is_empty(),
            "the new thread still remembers the old one"
        );
    }
}
