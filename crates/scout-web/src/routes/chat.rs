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
/// Where the mirror toggle's stored state goes.
///
/// Rendered rather than defaulted to "false", because the button is the
/// only place that state lives on the page: served as off while it is on,
/// the reader's first click posts `on` again — re-enabling something
/// already enabled and re-running the backfill — and turning it off takes
/// two presses. The stylesheet's own comment says the reader has to be able
/// to tell at a glance, and a hardcoded attribute cannot.
const MIRROR_STATE: &str = "<!--MIRROR-->";

pub fn routes(auth: AuthState) -> Router {
    Router::new()
        .route("/chat", get(chat))
        .route("/chat.js", get(client))
        .route("/chat/history", get(history))
        .route("/chat/messages", post(send_message))
        .route("/chat/mirror", post(mirror))
        .route("/chat/threads", get(list_threads).post(new_thread))
        .route("/chat/threads/{id}/open", post(open_thread))
        .route("/chat/threads/{id}/rename", post(rename_thread))
        .route("/chat/threads/{id}/pin", post(pin_thread))
        .route("/chat/threads/{id}/delete", post(delete_thread))
        .route("/chat/threads/{id}/title", post(suggest_title))
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
    let mirrored = matches!(scout_core::mirror::is_enabled(&auth.core, account_id).await, Ok(true));
    let page = TEMPLATE.replace(CSRF_TOKEN, &csrf).replace(MIRROR_STATE, if mirrored { "true" } else { "false" });
    // The same call that decides whether a run may promise a reminder
    // decides whether this is offered, so the two cannot disagree about
    // whether there is anywhere to send.
    let page = if reply_to_for(&auth, account_id).await.is_some() {
        page
    } else {
        strip_mirror_toggle(&page)
    };
    Html(page).into_response()
}

/// Removes the mirror control from the page.
///
/// A string edit rather than a template engine, because this page is a
/// static file with one conditional element in it and a dependency for one
/// `if` is a dependency to maintain forever. Returning the page unchanged
/// when the markers are missing means a restyle that renames the button
/// shows the control to everyone rather than serving a broken page — the
/// test below is what catches that instead.
fn strip_mirror_toggle(page: &str) -> String {
    let Some(start) = page.find(r#"<button id="mirror""#) else {
        return page.to_string();
    };
    let Some(end) = page[start..].find("</button>") else {
        return page.to_string();
    };
    let mut out = String::with_capacity(page.len());
    out.push_str(&page[..start]);
    out.push_str(&page[start + end + "</button>".len()..]);
    out
}

/// The page's client script.
///
/// `no-store` said here as well as by the signed-half layer, which inserts
/// the same value over the top of it. Not redundant where it counts: the
/// advice this script gives on a 422 is "reload to keep going", and that
/// only works if the reload fetches the script that stopped saying it. A
/// route moved to the public half — this one needs no session to serve —
/// would lose the layer and keep this.
async fn client() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        CLIENT,
    )
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
        // An error frame rather than a new status: the page already knows
        // how to show a sentence and clear the composer, which is all this
        // needs, and a status it has never heard of would show nothing.
        Ok(scout_core::run::RunOutcome::Overloaded) => End::Error {
            message: "Scout is busy with other people's requests right now. Try again in a minute."
                .to_string(),
        },
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
    /// Which thread this message belongs to, named by the page rather than
    /// inferred from the account.
    ///
    /// Inferring it means "whichever thread is newest at the moment the
    /// request lands", and the page is not the only thing that starts
    /// threads: a Telegram message, or another tab, can have started a
    /// newer one while this page sat open — and the reader's question would
    /// then be answered in a conversation they are not looking at, with the
    /// context of a conversation they never saw.
    thread: i64,
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

    // Before `open_thread`, on purpose: opening bumps a thread to current,
    // so checking the cap afterwards would let somebody who is capped
    // reorder their own sidebar by pressing send.
    if let Some(sentence) = scout_core::session::over_daily_cap(&auth.core, account_id).await {
        let (frames, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = frames.send(Frame::End(End::Error { message: sentence }));
        return sse_response(rx);
    }

    // Opening it is the ownership check and the bump to current, so the
    // run happens on exactly the thread the reader is looking at.
    let conversation_id = match scout_core::session::open_thread(&auth.core, account_id, body.thread).await {
        Ok(Some(_)) => body.thread,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "could not open a conversation");
            return sorry();
        }
    };

    let run = scout_api::RunContext {
        account_id,
        conversation_id,
        reply_to: reply_to_for(&auth, account_id).await,
        // On the web the body is the message: nothing is appended to it.
        title_source: Some(body.text.clone()),
    };
    let core = auth.core.clone();
    let text = body.text;
    let auth_for_mirror = auth.clone();

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
        // Queued before the end frame goes out, so the row is written
        // whether or not the reader's connection survived to see the
        // answer — a dropped stream still leaves the thread on their phone.
        if matches!(&outcome, Ok(scout_core::run::RunOutcome::Answered(_))) {
            // Reads the transcript `run_agent` has just saved, so what goes
            // to the phone is exactly what a reload would show — including
            // any answer the dead-link repair rewrote on the way out. Named
            // by id, not re-resolved: a run takes minutes, and the account's
            // current thread may have moved on while it ran.
            queue_conversation(&auth_for_mirror, account_id, conversation_id).await;
        }
        let _ = frames.send(Frame::End(end_frame(outcome)));
    });

    sse_response(rx)
}

/// The account and the CSRF check every thread route starts with.
///
/// One function rather than two lines repeated six times: a thread route
/// that forgot the second half would let any page on the internet rename or
/// delete a reader's threads, and the omission would be invisible.
async fn thread_caller(auth: &AuthState, headers: &HeaderMap) -> Result<i64, Response> {
    let account_id = admitted_account(auth, headers).await?;
    if !csrf_header_ok(auth, headers, account_id) {
        return Err(StatusCode::BAD_REQUEST.into_response());
    }
    Ok(account_id)
}

async fn list_threads(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    match scout_core::session::threads(&auth.core, account_id).await {
        Ok(list) => axum::Json(list).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "could not list threads");
            sorry()
        }
    }
}

/// Starts a thread and hands back the row the sidebar needs, rather than
/// the bare id: the page has just been told the list changed, and a second
/// round trip to find out what the new entry looks like is a list that
/// flickers empty in between.
async fn new_thread(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
) -> Response {
    let account_id = match thread_caller(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    // Before the new thread exists, not after: the divider belongs to the
    // conversation it closes, and once the new thread is started that
    // conversation is no longer the one `current_thread` returns.
    mirror_divider(&auth, account_id).await;
    let id = match scout_core::session::reset(&auth.core, account_id, "direct").await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "could not start a thread");
            return sorry();
        }
    };
    match scout_core::session::threads(&auth.core, account_id).await {
        Ok(list) => match list.into_iter().find(|t| t.id == id) {
            Some(thread) => axum::Json(thread).into_response(),
            None => sorry(),
        },
        Err(e) => {
            tracing::error!(error = %e, "could not read the new thread back");
            sorry()
        }
    }
}

async fn open_thread(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    headers: HeaderMap,
) -> Response {
    let account_id = match thread_caller(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    // Which thread they were on, read before opening changes the answer.
    // Clicking the row you are already reading is not a switch, and the
    // client re-opens the current thread on ordinary things — a tap in the
    // sidebar, the refresh after a delete — so a note for it would be a
    // line on the phone saying the reader moved to where they already were.
    let was = match scout_core::session::current_thread_id(&auth.core, account_id).await {
        Ok(current) => current,
        // Not fatal, and not a reason to refuse the open. An unknown
        // previous thread is treated as a switch: a note too many is a
        // smaller failure than an open that errors.
        Err(e) => {
            tracing::warn!(error = %e, account_id, "could not tell which thread was open");
            None
        }
    };
    match scout_core::session::open_thread(&auth.core, account_id, id).await {
        Ok(Some(turns)) => {
            if was != Some(id) {
                mirror_switch_note(&auth, account_id, id).await;
            }
            axum::Json(turns).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "could not open a thread");
            sorry()
        }
    }
}

#[derive(serde::Deserialize)]
struct RenameIn {
    title: String,
}

async fn rename_thread(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<RenameIn>,
) -> Response {
    let account_id = match thread_caller(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    // The blank rule lives in core, once; `Err` here is the database.
    match scout_core::session::rename(&auth.core, account_id, id, &body.title).await {
        Ok(scout_core::session::Renamed::Done) => StatusCode::NO_CONTENT.into_response(),
        Ok(scout_core::session::Renamed::NotFound) => StatusCode::NOT_FOUND.into_response(),
        Ok(scout_core::session::Renamed::Blank) => StatusCode::BAD_REQUEST.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "could not rename a thread");
            sorry()
        }
    }
}

#[derive(serde::Deserialize)]
struct PinIn {
    pinned: bool,
}

async fn pin_thread(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<PinIn>,
) -> Response {
    let account_id = match thread_caller(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    match scout_core::session::set_pinned(&auth.core, account_id, id, body.pinned).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "could not pin a thread");
            sorry()
        }
    }
}

async fn delete_thread(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    headers: HeaderMap,
) -> Response {
    let account_id = match thread_caller(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    match scout_core::session::delete_thread(&auth.core, account_id, id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "could not delete a thread");
            sorry()
        }
    }
}

#[derive(serde::Serialize)]
struct TitleOut {
    title: String,
}

async fn suggest_title(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    headers: HeaderMap,
) -> Response {
    let account_id = match thread_caller(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    // The one route on this page that spends money per press, and the
    // press is a click away in a list of rows. Ten in five minutes leaves
    // an honest reader naming every thread they have and bounds a stuck
    // client — or a happy clicker — at about a hundred and twenty model
    // calls an hour.
    if !auth.by_account.allow(&format!("title:{account_id}")) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    match scout_core::session::suggest_title(&auth.core, account_id, id).await {
        Ok(Some(title)) => axum::Json(TitleOut { title }).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        // A warning, not an error: the model gave nothing usable, which is
        // a thing models do and not a fault in the machine.
        //
        // JSON under a 502, not `sorry()`. This is asked for over `fetch`
        // and the answer is read by the page, which cannot do anything
        // with an HTML apology — and a 500 claims the machine broke for a
        // thing that is the far end declining to name a thread. The
        // database-shaped failures on the routes above keep `sorry()`,
        // because those really are the machine.
        Err(e) => {
            tracing::warn!(error = %e, account_id, "could not suggest a title");
            (StatusCode::BAD_GATEWAY, axum::Json(serde_json::json!({"error": "no title"})))
                .into_response()
        }
    }
}

/// Tells the phone which thread the browser switched to, when the mirror
/// is on. A note rather than a backfill: the whole thread is a tap away
/// on the laptop, and twenty paced messages is not a heads-up.
async fn mirror_switch_note(auth: &AuthState, account_id: i64, conversation_id: i64) {
    if !matches!(scout_core::mirror::is_enabled(&auth.core, account_id).await, Ok(true)) {
        return;
    }
    let Some(reply_to) = reply_to_for(auth, account_id).await else {
        return;
    };
    // One row's name, not the whole sidebar. Listing every thread and
    // then discarding all but one reads and expiry-checks the account's
    // entire history to render a single line.
    // A failed read is an unnamed thread here: the note is worth sending
    // under its generic name, and is not worth failing an open over.
    let title = scout_core::session::thread_title(&auth.core, account_id, conversation_id)
        .await
        .unwrap_or_default();
    let text = format!("── {} ──", title.unwrap_or_else(|| "New thread".to_string()));
    let at = chrono::Utc::now().timestamp();
    if let Err(e) = scout_core::mirror::note(&auth.core, account_id, &reply_to.address, &text, at).await {
        tracing::warn!(error = %e, account_id, "could not note the switch for Telegram");
    }
}

#[derive(serde::Deserialize)]
struct MirrorIn {
    on: bool,
}

/// Switches mirroring for this account, and backfills the current thread
/// when switching on.
///
/// Backfilling here rather than in the drain because this is where the
/// decision is made, and because it is cheap: it writes rows and returns.
/// The drain does the sending, so a twenty-row backfill is a fast database
/// write and a slow background delivery, not a slow request.
///
/// Enqueueing is idempotent, so ticking the box twice — or after a spell
/// with it off — costs nothing and cannot duplicate a message.
async fn mirror(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<MirrorIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Err(e) = scout_core::mirror::set_enabled(&auth.core, account_id, body.on).await {
        tracing::error!(error = %e, "could not switch the mirror");
        return sorry();
    }
    if body.on {
        queue_thread(&auth, account_id).await;
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Queues the reader's current thread, if they asked for that.
///
/// One function for both the backfill and keeping up, because enqueueing is
/// idempotent and the transcript is the only text this mirrors — so "send
/// the thread so far" and "send what just happened" are the same call, and
/// cannot drift apart.
///
/// Silently does nothing when the account has no Telegram identity: the
/// toggle is not shown in that case, so reaching here means a hand-made
/// request, and there is nowhere to send.
///
/// Failures are logged and swallowed. The answer is already on the reader's
/// screen and already in history; a mirror that did not get queued is worth
/// knowing about and is not worth failing a reply over.
async fn queue_thread(auth: &AuthState, account_id: i64) {
    if !matches!(scout_core::mirror::is_enabled(&auth.core, account_id).await, Ok(true)) {
        return;
    }
    let Some(reply_to) = reply_to_for(auth, account_id).await else {
        return;
    };
    if let Err(e) =
        scout_core::mirror::queue_thread(&auth.core, account_id, &reply_to.address).await
    {
        tracing::warn!(error = %e, account_id, "could not queue the thread for Telegram");
    }
}

/// The same, for a thread named by id rather than resolved by account.
///
/// What a finished run calls. `queue_thread` asks "what is this reader's
/// current thread", which is the right question for a backfill and the
/// wrong one afterwards: the run started on a particular conversation and
/// took minutes, and the current one may since have become another.
async fn queue_conversation(auth: &AuthState, account_id: i64, conversation_id: i64) {
    if !matches!(scout_core::mirror::is_enabled(&auth.core, account_id).await, Ok(true)) {
        return;
    }
    let Some(reply_to) = reply_to_for(auth, account_id).await else {
        return;
    };
    if let Err(e) =
        scout_core::mirror::queue_conversation(&auth.core, account_id, &reply_to.address, conversation_id)
            .await
    {
        tracing::warn!(error = %e, account_id, "could not queue the thread for Telegram");
    }
}

/// Marks the seam between two conversations in the mirrored chat.
///
/// Keyed to the conversation being *closed*, which is what gives it a turn
/// key nobody else will mint — so pressing the button twice cannot send the
/// same divider twice.
///
/// Skipped when that conversation had nothing in it. The second press of
/// "New thread" closes a thread nobody used, and a divider for that is two
/// dividers in a row with nothing between them.
///
/// Drawn with the same box-drawing rule as the switch note. Two seams in
/// one chat spelled two different ways read as two different kinds of
/// thing, and they are not: both say "a thread ended here".
async fn mirror_divider(auth: &AuthState, account_id: i64) {
    if !matches!(scout_core::mirror::is_enabled(&auth.core, account_id).await, Ok(true)) {
        return;
    }
    let Some(reply_to) = reply_to_for(auth, account_id).await else {
        return;
    };
    let Ok(Some((conversation_id, turns))) =
        scout_core::session::current_thread(&auth.core, account_id).await
    else {
        return;
    };
    if turns.is_empty() {
        return;
    }
    let seam = vec![scout_api::Turn {
        role: scout_api::Role::Scout,
        text: "── New thread ──".to_string(),
    }];
    if let Err(e) = scout_core::mirror::enqueue(
        &auth.core,
        account_id,
        &reply_to.address,
        conversation_id,
        &seam,
        false,
    )
    .await
    {
        tracing::warn!(error = %e, account_id, "could not queue a thread divider");
    }
}

#[cfg(test)]
mod tests {
    // Named imports rather than `use super::*`: the module imports axum's
    // `get` and `post`, which would shadow the test helpers of the same
    // name that every request in here goes through.
    use super::{end_frame, queue_thread, reply_to_for, AuthState};
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
    fn a_long_url_wraps_instead_of_widening_the_page() {
        // Answers are full of links, and `linkify` makes the whole url the
        // anchor's text. A percent-encoded one is a single token of 200-odd
        // characters with no whitespace, so `pre-wrap` alone cannot break it:
        // observed running out of the bubble, giving the transcript a
        // horizontal scrollbar and shifting the composer out of line.
        //
        // `break-word` would not do — it wraps but leaves the intrinsic
        // min-content width alone, and that width is what widens the page.
        // Scoped to the declaration, not the file. Written the loose way it
        // matched the word "anywhere" in the comment explaining the rule and
        // stayed green when the rule itself was weakened to `break-word` —
        // the second source-scan test today to assert against its own prose.
        let page = include_str!("../chat.html");
        let start = page.find(".turns li{").expect("the bubbles must be styled");
        let rule = &page[start..start + page[start..].find('}').expect("the rule must end")];
        assert!(
            rule.contains("overflow-wrap:anywhere"),
            "message bubbles must break an unbreakable token, or a long url widens the page"
        );
    }

    #[test]
    fn the_narration_cannot_squeeze_the_transcript_off_the_screen() {
        // `.status` holds the model's working, which on a real run is
        // thousands of words. It is a `flex:none` sibling of the scrolling
        // transcript, so without a ceiling it takes every pixel it asks
        // for: observed with `.turns` collapsed to zero height and the
        // answer nowhere on screen.
        //
        // Scoped to the declaration rather than the file. Both words appear
        // in the comment above the rule, and a file-wide `contains` would
        // stay green with the rule itself deleted — which is exactly the
        // way the url-wrapping test above was once wrong.
        let page = include_str!("../chat.html");
        let start = page.find(".status{").expect("the status line must be styled");
        let rule = &page[start..start + page[start..].find('}').expect("the rule must end")];
        assert!(
            rule.contains("max-height"),
            "the narration needs a ceiling, or it takes the whole column"
        );
        assert!(
            rule.contains("overflow-y:auto"),
            "a capped box must scroll, or the newest reasoning is unreachable"
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

    #[test]
    fn a_finished_run_queues_the_exchange_it_just_answered() {
        // `run_agent` needs a live model, so nothing in this workspace can
        // drive `send_message` end to end: deleting the call leaves every
        // other test in this file green, and the mirror would silently
        // never advance past its first backfill.
        //
        // Scoped to the handler's own body. Two source-scan tests written
        // today matched their own explanatory prose and could never fail;
        // slicing one function keeps the tests below — which name
        // `queue_conversation` repeatedly — out of range.
        let src = include_str!("chat.rs");
        let start = src.find("async fn send_message").expect("the handler must exist");
        let end = src[start..].find("\n}").expect("the handler must end") + start;
        assert!(
            src[start..end].contains("queue_conversation("),
            "a finished run must queue the thread, or the mirror only ever backfills"
        );
    }

    #[tokio::test]
    async fn a_completed_turn_is_queued_only_when_the_mirror_is_on() {
        // `run_agent` needs a live model, so this exercises the function the
        // run handler calls rather than driving a run.
        let (_app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        core.note_address(777, "telegram", "12345".to_string()).await.unwrap();
        // The thread the run has just saved — this reads the transcript now
        // rather than being handed the text, which is what keeps it and a
        // backfill sending the same thing.
        seed_conversation(&core, account_id, "cheapest beans", "here are three").await;
        let auth = auth_state(&core);

        queue_thread(&auth, account_id).await;
        assert!(
            scout_core::mirror::pending(&core, 10).await.unwrap().is_empty(),
            "queued a turn for someone who never asked for it"
        );

        scout_core::mirror::set_enabled(&core, account_id, true).await.unwrap();
        queue_thread(&auth, account_id).await;
        assert_eq!(scout_core::mirror::pending(&core, 10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_turn_is_not_queued_when_there_is_nowhere_to_send_it() {
        // Someone who switched the mirror on and later lost their delivery
        // address. Queueing rows nothing can deliver would fill the outbox
        // with work that fails five times each and is then abandoned.
        let (_app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        // Deliberately no `note_address`.
        seed_conversation(&core, account_id, "cheapest beans", "here are three").await;
        scout_core::mirror::set_enabled(&core, account_id, true).await.unwrap();
        let auth = auth_state(&core);
        queue_thread(&auth, account_id).await;
        assert!(scout_core::mirror::pending(&core, 10).await.unwrap().is_empty());
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

        for id in ["turns", "status", "notice", "ask", "text", "send", "reset", "threads", "menu", "side"] {
            assert!(page.contains(&format!(r#"id="{id}""#)), "the client binds to #{id}: {page}");
        }

        // Present is not enough: the reset control used to live in the
        // header, and a stray edit that moves it back would leave every
        // assertion above passing while the sidebar lost its first control.
        let side = page.find(r#"id="side""#).expect("the sidebar");
        let reset = page.find(r#"id="reset""#).expect("the reset form");
        let close = page.find("</aside>").expect("the sidebar closes");
        assert!(side < reset && reset < close, "New thread belongs in the sidebar, not the header");
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
        // A thread id the account does not own, deliberately: the refusal
        // must happen before anything looks the thread up, so a well-formed
        // body proves nothing and a token-less one is still refused.
        let res =
            post_json_with_cookie(&app, "/chat/messages", &cookie, None, r#"{"text":"hi","thread":1}"#).await;
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
            &app, "/chat/messages", &cookie, &csrf, "https://evil.example", r#"{"text":"hi","thread":1}"#,
        ).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn every_stream_ends_with_exactly_one_end_frame() {
        // Without it a client cannot tell a finished answer from a refusal
        // or a crash, and leaves a spinner up forever on two of the three.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        // A thread to send into: the message names the thread it belongs to
        // now, and one the account does not own is refused outright — which
        // would end the stream before it started and make this vacuous.
        let thread = scout_core::session::seed_exchange_for_tests(&core, account_id, "direct", "beans", "three")
            .await
            .unwrap();
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);
        let body = body_of(
            post_json_with_cookie(
                &app,
                "/chat/messages",
                &cookie,
                Some(&csrf),
                &format!(r#"{{"text":"hi","thread":{thread}}}"#),
            )
            .await,
        ).await;
        assert_eq!(body.matches("event: end").count(), 1, "got: {body}");
    }

    #[tokio::test]
    async fn turning_the_mirror_on_queues_the_thread_that_is_already_there() {
        // The point of backfilling: you tick the box because you are about
        // to pick the thread up on your phone, and a thread that starts
        // mid-story is not the thread.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        // Backfilling needs somewhere to send to. `admitted` only signs in;
        // it is the Telegram webhook's `note_address` that records a chat
        // to deliver into, so a test that skips it has nowhere for
        // `reply_to_for` to find and `pending` never rises off zero — the
        // same setup `a_run_promises_a_reminder_only_where_one_could_be_delivered`
        // needs for the same reason.
        core.note_address(777, "telegram", "12345".to_string()).await.unwrap();
        seed_conversation(&core, account_id, "cheapest beans", "here are three").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);
        let res =
            post_json_with_cookie(&app, "/chat/mirror", &cookie, Some(&csrf), r#"{"on":true}"#).await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(scout_core::mirror::pending(&core, 10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn turning_it_on_twice_queues_the_thread_once() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        core.note_address(777, "telegram", "12345".to_string()).await.unwrap();
        seed_conversation(&core, account_id, "cheapest beans", "here are three").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);
        for _ in 0..2 {
            post_json_with_cookie(&app, "/chat/mirror", &cookie, Some(&csrf), r#"{"on":true}"#).await;
        }
        assert_eq!(scout_core::mirror::pending(&core, 10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn the_mirror_cannot_be_switched_without_the_csrf_header() {
        // Same guard as /chat/messages and the thread routes: without it, any page
        // on the internet can turn a reader's chat into a Telegram feed.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let res = post_json_with_cookie(&app, "/chat/mirror", &cookie, None, r#"{"on":true}"#).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(!scout_core::mirror::is_enabled(&core, account_id).await.unwrap());
    }

    #[tokio::test]
    async fn the_mirror_toggle_is_absent_without_a_telegram_identity() {
        // A control that cannot work is a promise the page cannot keep. The
        // same call that decides whether a run may promise a reminder
        // decides whether this is shown, so the two cannot drift — and
        // `/chat/mirror`'s backfill quietly does nothing in this state,
        // which would be baffling if the button were there to press.
        let (app, core, _dir) = test_app_with_a_round().await;
        let scout_core::identity::SignIn::In { account_id: web_only } =
            scout_core::identity::sign_in(&core, "email", "ada@example.com").await.unwrap()
        else {
            panic!("the round had room");
        };
        let cookie = crate::session::mint(TEST_KEY, web_only, DAY);
        let page = body_of(get_with_cookie(&app, "/chat", &cookie).await).await;
        assert!(!page.contains(r#"id="mirror""#), "offered a mirror with nowhere to send it");
    }

    #[tokio::test]
    async fn the_page_without_a_mirror_toggle_is_still_a_whole_page() {
        // Stripping means a reader with no Telegram gets different markup
        // from everyone else, and `the_page_still_carries_every_id_the
        // _client_binds_to` only ever sees the unstripped one. A cut that
        // took the reset form or the composer with it would pass every
        // other test in this file and break the page for exactly the people
        // who cannot mirror.
        let (app, core, _dir) = test_app_with_a_round().await;
        let scout_core::identity::SignIn::In { account_id: web_only } =
            scout_core::identity::sign_in(&core, "email", "ada@example.com").await.unwrap()
        else {
            panic!("the round had room");
        };
        let cookie = crate::session::mint(TEST_KEY, web_only, DAY);
        let page = body_of(get_with_cookie(&app, "/chat", &cookie).await).await;
        for id in ["turns", "status", "notice", "ask", "text", "send", "reset"] {
            assert!(page.contains(&format!(r#"id="{id}""#)), "the client binds to #{id}");
        }
        assert!(!page.contains(r#"id="mirror""#));
        // And the cut left no orphaned markup behind it.
        assert_eq!(page.matches("<div class=\"controls\">").count(), 1);
        // The paper-plane path, not the viewBox: the send button's icon
        // shares `viewBox="0 0 24 24"`, so the looser check failed against
        // a page that had stripped correctly.
        assert!(!page.contains("M21.7 3.4"), "the toggle's icon outlived the toggle");
    }

    #[tokio::test]
    async fn the_toggle_shows_the_state_it_is_actually_in() {
        // The button is the only place this state lives on the page. Served
        // as off while it is on, the first click posts `on` again — which
        // re-runs the backfill — and turning it off takes two presses.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        core.note_address(777, "telegram", "12345".to_string()).await.unwrap();
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);

        // Matched on the button, not the bare attribute: the stylesheet
        // carries `#mirror[aria-pressed="true"]`, so the loose check passed
        // against a page that hardcoded the state — the third assertion
        // today to match a string that also lives somewhere else.
        let on = r#"id="mirror" type="button" aria-pressed="true""#;
        let off = r#"id="mirror" type="button" aria-pressed="false""#;

        let page = body_of(get_with_cookie(&app, "/chat", &cookie).await).await;
        assert!(page.contains(off), "off is not shown as off");

        scout_core::mirror::set_enabled(&core, account_id, true).await.unwrap();
        let page = body_of(get_with_cookie(&app, "/chat", &cookie).await).await;
        assert!(page.contains(on), "on is shown as off");
        assert!(!page.contains("<!--MIRROR-->"), "the placeholder reached the browser");
    }

    #[tokio::test]
    async fn the_mirror_toggle_is_offered_to_someone_on_telegram() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        core.note_address(777, "telegram", "12345".to_string()).await.unwrap();
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let page = body_of(get_with_cookie(&app, "/chat", &cookie).await).await;
        assert!(page.contains(r#"id="mirror""#), "no way to switch the mirror on");
    }

    #[tokio::test]
    async fn a_new_thread_does_not_remember_the_last_one() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        seed_conversation(&core, account_id, "cheapest beans", "here are three").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);

        let res = post_json_with_cookie(&app, "/chat/threads", &cookie, Some(&csrf), "").await;
        assert_eq!(res.status(), StatusCode::OK);

        assert!(
            scout_core::session::transcript(&core, account_id).await.unwrap().is_empty(),
            "the new thread still remembers the old one"
        );
    }

    #[tokio::test]
    async fn pressing_new_thread_twice_does_not_queue_two_seams() {
        // The second press closes a thread nobody said anything in, and a
        // divider closing an empty thread is just two dividers in a row.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        core.note_address(777, "telegram", "12345".to_string()).await.unwrap();
        seed_conversation(&core, account_id, "cheapest beans", "here are three").await;
        scout_core::mirror::set_enabled(&core, account_id, true).await.unwrap();
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);

        post_json_with_cookie(&app, "/chat/threads", &cookie, Some(&csrf), "").await;
        post_json_with_cookie(&app, "/chat/threads", &cookie, Some(&csrf), "").await;

        let seams = scout_core::mirror::pending(&core, 10)
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.body.contains("New thread"))
            .count();
        assert_eq!(seams, 1, "queued a divider for a thread nobody used");
    }

    #[tokio::test]
    async fn a_new_thread_queues_nothing_when_the_mirror_is_off() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        core.note_address(777, "telegram", "12345".to_string()).await.unwrap();
        seed_conversation(&core, account_id, "cheapest beans", "here are three").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);

        post_json_with_cookie(&app, "/chat/threads", &cookie, Some(&csrf), "").await;

        assert!(scout_core::mirror::pending(&core, 10).await.unwrap().is_empty());
    }

    async fn threads_json(app: &axum::Router, cookie: &str) -> serde_json::Value {
        serde_json::from_str(&body_of(get_with_cookie(app, "/chat/threads", cookie).await).await).unwrap()
    }

    #[tokio::test]
    async fn the_thread_list_is_the_accounts_direct_threads_with_one_current() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        seed_conversation(&core, account_id, "beans", "three").await;
        seed_conversation(&core, account_id, "hubs", "two").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);

        let list = threads_json(&app, &cookie).await;
        let list = list.as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list.iter().filter(|t| t["current"] == true).count(), 1);
        assert!(list[0]["current"] == true, "newest is current and first: {list:?}");
    }

    #[tokio::test]
    async fn opening_a_thread_returns_its_transcript_and_makes_it_current() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let older = scout_core::session::seed_exchange_for_tests(&core, account_id, "direct", "beans", "three").await.unwrap();
        seed_conversation(&core, account_id, "hubs", "two").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);

        let res = post_json_with_cookie(&app, &format!("/chat/threads/{older}/open"), &cookie, Some(&csrf), "").await;
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_of(res).await;
        assert!(body.contains("beans"), "got: {body}");

        let list = threads_json(&app, &cookie).await;
        assert_eq!(list[0]["id"], older);
        assert_eq!(list[0]["current"], true);
    }

    #[tokio::test]
    async fn someone_elses_thread_is_not_found_on_every_route() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let owner = admitted(&core, "777").await;
        let theirs = scout_core::session::seed_exchange_for_tests(&core, owner, "direct", "beans", "three").await.unwrap();
        let me = admitted(&core, "888").await;
        let cookie = crate::session::mint(TEST_KEY, me, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, me);

        for (path, body) in [
            ("open", ""),
            ("rename", r#"{"title":"mine"}"#),
            ("pin", r#"{"pinned":true}"#),
            ("delete", ""),
            ("title", ""),
        ] {
            let res = post_json_with_cookie(&app, &format!("/chat/threads/{theirs}/{path}"), &cookie, Some(&csrf), body).await;
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path} answered {}", res.status());
        }
        assert_eq!(
            scout_core::session::transcript(&core, owner).await.unwrap().len(),
            2,
            "a stranger changed the owner's thread"
        );
    }

    #[tokio::test]
    async fn rename_pin_and_delete_change_what_the_list_says() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let id = scout_core::session::seed_exchange_for_tests(&core, account_id, "direct", "beans", "three").await.unwrap();
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);

        let res = post_json_with_cookie(&app, &format!("/chat/threads/{id}/rename"), &cookie, Some(&csrf), r#"{"title":"cheapest beans"}"#).await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let res = post_json_with_cookie(&app, &format!("/chat/threads/{id}/pin"), &cookie, Some(&csrf), r#"{"pinned":true}"#).await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let list = threads_json(&app, &cookie).await;
        assert_eq!(list[0]["title"], "cheapest beans");
        assert_eq!(list[0]["pinned"], true);

        let res = post_json_with_cookie(&app, &format!("/chat/threads/{id}/delete"), &cookie, Some(&csrf), "").await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert!(threads_json(&app, &cookie).await.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_empty_rename_is_refused() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let id = scout_core::session::seed_exchange_for_tests(&core, account_id, "direct", "beans", "three").await.unwrap();
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);
        let res = post_json_with_cookie(&app, &format!("/chat/threads/{id}/rename"), &cookie, Some(&csrf), r#"{"title":"  "}"#).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_new_thread_is_returned_as_a_thread_and_is_current() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        seed_conversation(&core, account_id, "beans", "three").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);

        let res = post_json_with_cookie(&app, "/chat/threads", &cookie, Some(&csrf), "").await;
        assert_eq!(res.status(), StatusCode::OK);
        let thread: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_eq!(thread["current"], true);
        assert!(thread["title"].is_null());
        assert_eq!(threads_json(&app, &cookie).await.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_message_into_a_thread_that_is_not_yours_is_refused_before_anything_runs() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let owner = admitted(&core, "777").await;
        let theirs = scout_core::session::seed_exchange_for_tests(&core, owner, "direct", "beans", "three").await.unwrap();
        let me = admitted(&core, "888").await;
        let cookie = crate::session::mint(TEST_KEY, me, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, me);

        let res = post_json_with_cookie(
            &app, "/chat/messages", &cookie, Some(&csrf), &format!(r#"{{"text":"hi","thread":{theirs}}}"#),
        ).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn switching_threads_tells_the_phone_which_one_by_name() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        core.note_address(777, "telegram", "12345".to_string()).await.unwrap();
        let older = scout_core::session::seed_exchange_for_tests(&core, account_id, "direct", "beans", "three").await.unwrap();
        scout_core::session::rename(&core, account_id, older, "cheapest beans").await.unwrap();
        seed_conversation(&core, account_id, "hubs", "two").await;
        scout_core::mirror::set_enabled(&core, account_id, true).await.unwrap();
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);

        let res = post_json_with_cookie(&app, &format!("/chat/threads/{older}/open"), &cookie, Some(&csrf), "").await;
        assert_eq!(res.status(), StatusCode::OK);

        let pending = scout_core::mirror::pending(&core, 10).await.unwrap();
        assert!(pending.iter().any(|p| p.body == "── cheapest beans ──"), "no note about the switch: {pending:?}");
    }

    #[tokio::test]
    async fn opening_the_thread_you_are_on_sends_nothing_to_the_phone() {
        // Clicking the row you are already reading is not a switch, and a
        // divider for it is a line in the Telegram chat saying the reader
        // moved to where they already were. The client re-opens the current
        // thread on ordinary things — a tap in the sidebar, a refresh after
        // a delete — so this is not a rare case.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        core.note_address(777, "telegram", "12345".to_string()).await.unwrap();
        let only = scout_core::session::seed_exchange_for_tests(&core, account_id, "direct", "beans", "three")
            .await
            .unwrap();
        scout_core::mirror::set_enabled(&core, account_id, true).await.unwrap();
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);

        let res = post_json_with_cookie(&app, &format!("/chat/threads/{only}/open"), &cookie, Some(&csrf), "").await;
        assert_eq!(res.status(), StatusCode::OK);

        let pending = scout_core::mirror::pending(&core, 10).await.unwrap();
        assert!(
            pending.is_empty(),
            "opening the thread already open announced a switch: {:?}",
            pending.iter().map(|p| &p.body).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn starting_a_new_thread_marks_the_seam_in_telegram() {
        // Without it, scrolling back through Telegram runs two unrelated
        // conversations together with nothing between them, which is
        // precisely the continuity the mirror is for.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        core.note_address(777, "telegram", "12345".to_string()).await.unwrap();
        seed_conversation(&core, account_id, "cheapest beans", "here are three").await;
        scout_core::mirror::set_enabled(&core, account_id, true).await.unwrap();
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);

        let res = post_json_with_cookie(&app, "/chat/threads", &cookie, Some(&csrf), "").await;
        assert_eq!(res.status(), StatusCode::OK);

        let queued = scout_core::mirror::pending(&core, 10).await.unwrap();
        assert!(
            queued.iter().any(|r| r.body == "── New thread ──"),
            "no seam between two conversations: {:?}",
            queued.iter().map(|r| &r.body).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn every_thread_post_refuses_a_request_without_the_csrf_header() {
        // `someone_elses_thread_is_not_found_on_every_route` proves the
        // ownership half on every route; this is the other half, on the
        // account's own thread, where a missing check would actually do
        // something. Without it any page on the internet can rename, pin
        // or delete a reader's threads, or spend their model calls.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let mine = scout_core::session::seed_exchange_for_tests(&core, account_id, "direct", "beans", "three")
            .await
            .unwrap();
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);

        for (path, body) in [
            ("open", ""),
            ("rename", r#"{"title":"mine"}"#),
            ("pin", r#"{"pinned":true}"#),
            ("delete", ""),
            ("title", ""),
        ] {
            let res =
                post_json_with_cookie(&app, &format!("/chat/threads/{mine}/{path}"), &cookie, None, body).await;
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{path} answered {}", res.status());
        }
        assert_eq!(
            scout_core::session::transcript(&core, account_id).await.unwrap().len(),
            2,
            "a token-less request changed the thread anyway"
        );
    }

    #[tokio::test]
    async fn a_title_the_model_could_not_give_is_a_502_the_page_can_read() {
        // The client asks for this over `fetch` and shows a notice from
        // what comes back. `sorry()` answers an HTML apology page under a
        // 500, which the page cannot read and which says "the machine
        // broke" for a thing that is a model declining to name a thread.
        //
        // End to end on purpose, model call included — and the model this
        // harness is configured with is a closed port on loopback
        // (`MINIMAX_BASE_URL` in `build_app`), so the call fails on connect
        // in microseconds and nothing goes onto the network.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let mine = scout_core::session::seed_exchange_for_tests(&core, account_id, "direct", "beans", "three")
            .await
            .unwrap();
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);

        let res =
            post_json_with_cookie(&app, &format!("/chat/threads/{mine}/title"), &cookie, Some(&csrf), "").await;
        assert_eq!(res.status(), StatusCode::BAD_GATEWAY, "no model is reachable from a test");
        let body = body_of(res).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap_or_else(|_| panic!("not JSON: {body}"));
        assert!(json.get("error").is_some(), "the page has nothing to read: {body}");
    }

    #[tokio::test]
    async fn the_eleventh_title_request_in_five_minutes_is_refused() {
        // Every press of ✦ is a model call somebody pays for, and the
        // button is a click away in a list of threads. A stuck client
        // retrying, or a reader enjoying themselves, must not be able to
        // spend without a ceiling.
        //
        // An empty thread, because what is under test is the counter and
        // not the model: with no messages `suggest_title` returns
        // `Ok(None)` and the route answers 404 before it would call one.
        // The limiter is checked first either way, so ten 404s spend ten
        // of the quota and the eleventh press is refused — the same shape
        // as ten real titles, with nothing to reach for.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let mine = scout_core::session::reset(&core, account_id, "direct").await.unwrap();
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);
        let uri = format!("/chat/threads/{mine}/title");

        for n in 1..=10 {
            let res = post_json_with_cookie(&app, &uri, &cookie, Some(&csrf), "").await;
            assert_eq!(
                res.status(),
                StatusCode::NOT_FOUND,
                "request {n} of the quota answered {} — an empty thread has no title to give",
                res.status()
            );
        }
        let res = post_json_with_cookie(&app, &uri, &cookie, Some(&csrf), "").await;
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS, "the eleventh call was paid for");
    }

    #[test]
    fn a_message_runs_on_the_thread_the_page_named_not_on_whatever_is_newest() {
        // `run_agent` needs a live model, so the join is asserted from the
        // source: the handler must open the named thread and must not fall
        // back to `resolve_conversation`, which picks the newest — the
        // race this field exists to close. And it must mirror the thread
        // that ran, not re-resolve one after the fact.
        let src = include_str!("chat.rs");
        let start = src.find("async fn send_message").expect("the handler must exist");
        let end = src[start..].find("\n}\n").expect("the handler must end") + start;
        let body = &src[start..end];
        assert!(body.contains("open_thread("), "the message does not go to the named thread");
        assert!(!body.contains("resolve_conversation("), "the message can still land in the newest thread");
        assert!(body.contains("queue_conversation("), "the mirror re-resolves the thread after the run");
        assert!(!body.contains("queue_thread("), "the mirror could send a different thread than the one that ran");
    }

    #[test]
    fn the_client_names_the_thread_it_is_sending_into() {
        // `MessageIn.thread` has no default: a client that omits it gets a
        // 422 and the page shows "could not be reached" for every message.
        let js = include_str!("../chat.js");
        let start = js.find("fetch('/chat/messages'").expect("the client must post messages");
        let end = js[start..].find("})").expect("the call must end") + start;
        assert!(js[start..end].to_lowercase().contains("thread"), "the send body names no thread");
    }

    #[test]
    fn a_list_refresh_never_moves_the_composer_by_itself() {
        // The server's `current` is whichever thread was touched last
        // anywhere — the phone, another tab, a run in another thread that
        // just appended its answer. A refresh that adopted it would
        // retarget the composer while the reader is looking at a different
        // transcript, and the next message would be answered in a thread
        // nobody is reading. `resolveCurrent` is where that rule lives, and
        // it is a pure function with its own tests — this is the assertion
        // that the refresh actually goes through it.
        let js = include_str!("../chat.js");
        let start = js.find("async function refreshThreads").expect("the client must refresh its list");
        let end = js[start..].find("\n  }\n").expect("the function must end") + start;
        let body = &js[start..end];
        assert!(body.contains("resolveCurrent("), "the refresh decides the composer's thread by hand");
        assert!(
            !body.contains("currentThread = current"),
            "the refresh adopts whatever the server calls current"
        );
    }

    #[test]
    fn a_threads_row_can_be_acted_on_from_the_keyboard() {
        // The tools are hidden until `:hover` or `.current`, and neither
        // happens to a keyboard. Tabbing to a non-current row's rename
        // button would move focus to something `display:none` — which is
        // to say nowhere, and the row would be unreachable without a mouse.
        let css = include_str!("../chat.html");
        assert!(css.contains("li:focus-within .tools"), "a keyboard cannot reach a non-current row's tools");
    }

    #[test]
    fn a_threads_name_is_the_biggest_thing_on_its_row() {
        // A thread is found by its name, and in a 240px column the name was
        // drawn at the same size as the four tool glyphs beside it and cut
        // to a one-line ellipsis — "Haribo Ho…" on the current row, where
        // the tools are always showing. Two lines and a larger type size
        // are what make the row scannable.
        //
        // Scoped to the declaration rather than the file: both properties
        // are named in the comment above the rule, and a file-wide
        // `contains` would stay green with the rule itself deleted.
        let page = include_str!("../chat.html");
        let start = page.find(".threads .title{").expect("the title must be styled");
        let rule = &page[start..start + page[start..].find('}').expect("the rule must end")];
        assert!(
            rule.contains("-webkit-line-clamp"),
            "a thread's name is cut to one line, so a long one is unreadable"
        );
        assert!(
            !rule.contains("white-space:nowrap"),
            "a clamped title still refuses to wrap, so it can only ever show one line"
        );

        // And a row's tools can take a line of their own rather than eat
        // the title's width — which is what the current row, the one whose
        // tools are permanently on screen, depends on.
        let start = page.find(".threads li{").expect("a thread row must be styled");
        let rule = &page[start..start + page[start..].find('}').expect("the rule must end")];
        assert!(
            rule.contains("flex-wrap:wrap"),
            "a row cannot drop its tools onto their own line"
        );
    }

    #[tokio::test]
    async fn the_client_script_is_never_cached() {
        // The 422 arm's advice is "reload to keep going", and a reload that
        // re-served the stale script from cache would leave the page giving
        // the same advice forever.
        let (app, _core, _dir) = test_app().await;
        let res = get(&app, "/chat.js").await;
        assert_eq!(
            res.headers()[axum::http::header::CACHE_CONTROL].to_str().unwrap(),
            "no-store",
            "a reload can be answered from cache"
        );
    }

    #[test]
    fn a_message_the_page_could_not_send_is_taken_off_the_screen() {
        // The 422 arm is the page finding out its own request no longer
        // makes sense — the send body no longer matches what the server
        // wants. The bubble the submit handler already appended for the
        // words has to come off by hand here, or they sit twice on the
        // page: once back in the composer, once in a turn that was never
        // asked.
        let js = include_str!("../chat.js");
        let start = js.find("res.status === 422").expect("the 422 arm must exist");
        let end = js[start..].find('}').expect("the arm must end") + start;
        let body = &js[start..end];
        assert!(body.contains("retract()"), "a refused send leaves its bubble on the screen");
    }

    #[test]
    fn the_expiry_countdown_ticks_only_while_the_tab_is_visible() {
        // A hidden tab's labels are seen by nobody, so ticking them anyway
        // is a request nobody asked for — `visibilitychange` refreshes the
        // list outright when the tab comes back into view instead.
        let js = include_str!("../chat.js");
        let start = js.find("function tickWhenLabels").expect("the ticker must exist");
        let end = js[start..].find("\n  }\n").expect("the function must end") + start;
        let body = &js[start..end];
        assert!(body.contains("document.hidden"), "the ticker does not check tab visibility");
        assert!(body.contains(".when"), "the ticker does not touch the when labels");
    }

    #[tokio::test]
    async fn the_reset_route_is_gone() {
        // The sidebar starts threads now; nothing on the page posts here
        // any more. A stale bookmark, or an old cached page, finding the
        // route gone — rather than it quietly doing something else — is
        // the point.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);

        let res = post_json_with_cookie(&app, "/chat/reset", &cookie, Some(&csrf), "").await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "the reset route answered {}", res.status());
    }
}
