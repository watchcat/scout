//! Telegram delivering updates to us, instead of us asking for them.
//!
//! The intake is our own twenty lines rather than teloxide's
//! `webhooks::axum` for three reasons, each small and each on the one door
//! a stranger can knock on: it compares the secret with `==` (its own
//! FIXME says so), it writes the whole body of an update it cannot parse
//! to the log — which is somebody's message — and it would pull Telegram
//! into `scout-web`, which only mounts the router this hands it.
//!
//! Shutdown is the part that matters. The dispatcher stops a listener by
//! its token and then waits for the stream to end; this stream closes the
//! queue at that moment, hands over what was already accepted, and ends.
//! Anything Telegram offers after that is answered 503, and Telegram keeps
//! it and offers it again — to the next pod, once it is up.

use axum::{body::Bytes, extract::State, http::HeaderMap, http::StatusCode, routing::post, Router};
use futures_util::stream::{self, BoxStream, StreamExt};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use teloxide::stop::{mk_stop_token, StopFlag, StopToken};
use teloxide::types::Update;
use teloxide::update_listeners::{AsUpdateStream, UpdateListener};
use tokio::sync::mpsc;

/// Where Telegram posts. `TELEGRAM_WEBHOOK_URL` must end in exactly this,
/// or the address Telegram is given and the route we serve are two
/// different things and every update is a 404 nobody reads.
pub const PATH: &str = "/telegram/webhook";

/// Updates accepted but not yet taken by the dispatcher. Past this the
/// intake answers 503 and Telegram retries, which is the backpressure a
/// burst should meet — not an unbounded queue growing in a pod with a
/// memory limit.
const QUEUE: usize = 256;

/// The value Telegram sends back in `X-Telegram-Bot-Api-Secret-Token`.
///
/// Derived from the bot token rather than configured: one fewer secret to
/// keep in `.env` and in step with the deployment, and it rotates when the
/// token does, which is exactly when it should. Hex, because Telegram
/// allows only `A-Z a-z 0-9 _ -` and at most 256 characters.
pub fn secret(bot_token: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(bot_token.as_bytes()).expect("HMAC takes any key");
    mac.update(b"scout telegram webhook");
    mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// How long an update may wait in the queue before the bot is declared
/// stuck. Handlers run off the queue in their own tasks, so the queue is
/// drained in milliseconds when the dispatcher is alive; an update still
/// waiting after this is one nothing is going to take.
pub const STALL: Duration = Duration::from_secs(120);

/// What the intake knows about the dispatcher behind it, for `/healthz`.
///
/// The failure this catches is the one nothing caught before: a bot the
/// updates reach and the dispatcher no longer takes — a wedged worker
/// queue, a stream that ended without the process ending. Telegram keeps
/// redelivering into a full queue, the page stays up, and from outside
/// the bot has simply gone quiet. Telegram being unreachable is not this:
/// then nothing arrives, the queue is empty, and a restart would change
/// nothing, so a quiet bot is a healthy one.
pub struct Health {
    queued: AtomicUsize,
    /// Since when something has been waiting: set when the queue goes
    /// from empty to not, cleared when it empties. Measured from that and
    /// not from the last take, or a bot quiet for an hour that then hears
    /// one update would count the hour against it.
    waiting_since: std::sync::Mutex<Option<tokio::time::Instant>>,
    stall: Duration,
}

impl Health {
    fn new(stall: Duration) -> Self {
        Self { queued: AtomicUsize::new(0), waiting_since: std::sync::Mutex::new(None), stall }
    }

    fn accepted(&self) {
        self.queued.fetch_add(1, Ordering::Relaxed);
        let mut since = self.waiting_since.lock().unwrap_or_else(|e| e.into_inner());
        if since.is_none() {
            *since = Some(tokio::time::Instant::now());
        }
    }

    fn taken(&self) {
        let left = self.queued.fetch_sub(1, Ordering::Relaxed).saturating_sub(1);
        let mut since = self.waiting_since.lock().unwrap_or_else(|e| e.into_inner());
        // The rest have waited at most since now, as far as this knows;
        // a stall is then declared `stall` after the last take, which is
        // the conservative side to be wrong on.
        *since = (left > 0).then(tokio::time::Instant::now);
    }

    /// Why the bot is stuck, or `None`.
    pub fn stalled(&self) -> Option<String> {
        let since = (*self.waiting_since.lock().unwrap_or_else(|e| e.into_inner()))?;
        let waited = since.elapsed();
        (waited >= self.stall).then(|| {
            format!("{} update(s) waiting and none taken for {}s", self.queued.load(Ordering::Relaxed), waited.as_secs())
        })
    }
}

#[derive(Clone)]
struct Intake {
    secret: String,
    queue: mpsc::Sender<Update>,
    health: Arc<Health>,
}

/// The route to mount and the listener to hand the dispatcher.
pub fn intake(secret: String) -> (Router, Listener) {
    intake_with(secret, STALL)
}

fn intake_with(secret: String, stall: Duration) -> (Router, Listener) {
    let (queue, rx) = mpsc::channel(QUEUE);
    let (token, flag) = mk_stop_token();
    let health = Arc::new(Health::new(stall));
    let router = Router::new().route(PATH, post(receive)).with_state(Intake { secret, queue, health: health.clone() });
    (router, Listener { rx, token, flag, closing: false, health })
}

async fn receive(State(intake): State<Intake>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let given = headers
        .get("x-telegram-bot-api-secret-token")
        .map(|v| v.as_bytes())
        .unwrap_or_default();
    if !constant_time_eq(given, intake.secret.as_bytes()) {
        return StatusCode::UNAUTHORIZED;
    }
    let update: Update = match serde_json::from_slice(&body) {
        Ok(update) => update,
        Err(e) => {
            // The category and the place, never the error's text: serde
            // quotes the value it choked on, and here that value is
            // somebody's message. 200, because Telegram would otherwise
            // offer the same unreadable update again forever.
            tracing::warn!(kind = ?e.classify(), line = e.line(), column = e.column(), "an update we could not read");
            return StatusCode::OK;
        }
    };
    // One line per update, naming what it is and never what it says: the
    // only record that Telegram reached us at all, which is the first
    // question when the bot seems to have gone quiet.
    let kind = match &update.kind {
        teloxide::types::UpdateKind::Message(_) => "message",
        teloxide::types::UpdateKind::MessageReaction(_) => "reaction",
        _ => "other",
    };
    tracing::info!(update_id = update.id.0, kind, "update in");
    match intake.queue.try_send(update) {
        Ok(()) => {
            intake.health.accepted();
            StatusCode::OK
        }
        // Full, or closed because the dispatcher is shutting down. Either
        // way the update is not ours yet, and saying so is what makes
        // Telegram keep it.
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub struct Listener {
    rx: mpsc::Receiver<Update>,
    token: StopToken,
    flag: StopFlag,
    closing: bool,
    health: Arc<Health>,
}

impl Listener {
    /// For `/healthz`: shared with the intake, read by the probe.
    pub fn health(&self) -> Arc<Health> {
        self.health.clone()
    }
}

impl<'a> AsUpdateStream<'a> for Listener {
    type StreamErr = Infallible;
    type Stream = BoxStream<'a, Result<Update, Infallible>>;

    fn as_stream(&'a mut self) -> Self::Stream {
        stream::unfold(self, |this| async move {
            if !this.closing {
                tokio::select! {
                    biased;
                    _ = &mut this.flag => {
                        // No more in; what is already in still goes out.
                        // `recv` below returns the queue's remainder and
                        // then `None`, which ends the stream, which is
                        // what lets the dispatcher finish.
                        this.closing = true;
                        this.rx.close();
                    }
                    update = this.rx.recv() => {
                        if update.is_some() { this.health.taken() }
                        return update.map(|u| (Ok(u), this))
                    }
                }
            }
            let update = this.rx.recv().await;
            if update.is_some() {
                this.health.taken();
            }
            update.map(|u| (Ok(u), this))
        })
        .boxed()
    }
}

impl UpdateListener for Listener {
    type Err = Infallible;

    fn stop_token(&mut self) -> StopToken {
        self.token.clone()
    }
}

/// What Telegram thinks of the webhook, every so often: a backlog on its
/// side, or an error it met delivering, is the other half of "the bot
/// has gone quiet", and it is only visible from there. Logged, not acted
/// on — a restart here fixes nothing on Telegram's side — so the line is
/// there when somebody asks why.
pub async fn watch(bot: teloxide::Bot, every: Duration) {
    use teloxide::prelude::Requester;
    let mut tick = tokio::time::interval(every);
    tick.tick().await;
    loop {
        tick.tick().await;
        match bot.get_webhook_info().await {
            Ok(info) => {
                let stale_error = info.last_error_date.is_some_and(|at| (chrono::Utc::now() - at).num_seconds() < every.as_secs() as i64);
                if info.pending_update_count > 0 || stale_error {
                    tracing::warn!(
                        pending = info.pending_update_count,
                        last_error = info.last_error_message.as_deref().unwrap_or("none"),
                        "Telegram is holding updates for this bot"
                    );
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not read the webhook's state from Telegram"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const UPDATE: &str = r#"{"update_id":7,"message":{"message_id":5,"date":1700000000,
        "chat":{"id":42,"type":"private","first_name":"Sasha"},
        "from":{"id":42,"is_bot":false,"first_name":"Sasha"},"text":"hello"}}"#;

    async fn post(router: &Router, secret: Option<&str>, body: &str) -> StatusCode {
        let mut req = Request::post(PATH).header("content-type", "application/json");
        if let Some(s) = secret {
            req = req.header("x-telegram-bot-api-secret-token", s);
        }
        router.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap().status()
    }

    #[test]
    fn the_secret_is_one_telegram_accepts_and_follows_the_token() {
        let s = secret("123:abc");
        assert_eq!(s.len(), 64);
        assert!(s.bytes().all(|b| b.is_ascii_alphanumeric()));
        assert_eq!(s, secret("123:abc"));
        assert_ne!(s, secret("123:abd"));
    }

    #[tokio::test]
    async fn an_update_without_the_secret_is_refused_and_never_queued() {
        let (router, mut listener) = intake("right".to_string());
        assert_eq!(post(&router, None, UPDATE).await, StatusCode::UNAUTHORIZED);
        assert_eq!(post(&router, Some("wrong"), UPDATE).await, StatusCode::UNAUTHORIZED);
        assert_eq!(post(&router, Some("righ"), UPDATE).await, StatusCode::UNAUTHORIZED);
        assert!(listener.rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn an_update_with_the_secret_reaches_the_dispatcher() {
        let (router, mut listener) = intake("right".to_string());
        assert_eq!(post(&router, Some("right"), UPDATE).await, StatusCode::OK);
        let update = listener.as_stream().next().await.unwrap().unwrap();
        assert_eq!(update.id.0, 7);
    }

    #[tokio::test]
    async fn an_unreadable_update_is_acknowledged_and_dropped() {
        let (router, mut listener) = intake("right".to_string());
        assert_eq!(post(&router, Some("right"), r#"{"update_id":"#).await, StatusCode::OK);
        assert!(listener.rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn stopping_hands_over_what_was_accepted_then_ends_and_turns_new_ones_away() {
        let (router, mut listener) = intake("right".to_string());
        assert_eq!(post(&router, Some("right"), UPDATE).await, StatusCode::OK);
        assert_eq!(post(&router, Some("right"), &UPDATE.replace("\"update_id\":7", "\"update_id\":8")).await, StatusCode::OK);
        listener.stop_token().stop();

        let mut stream = listener.as_stream();
        let ids: Vec<i32> = (&mut stream).map(|u| u.unwrap().id.0 as i32).collect().await;
        assert_eq!(ids, vec![7, 8], "accepted before the stop, so still delivered");
        drop(stream);
        // Telegram keeps what it is told was not taken, and offers it to
        // the next pod.
        assert_eq!(post(&router, Some("right"), UPDATE).await, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn a_full_queue_is_a_retry_not_a_loss() {
        let (router, _listener) = intake("right".to_string());
        for _ in 0..QUEUE {
            assert_eq!(post(&router, Some("right"), UPDATE).await, StatusCode::OK);
        }
        assert_eq!(post(&router, Some("right"), UPDATE).await, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn a_real_dispatcher_hands_a_delivered_update_to_its_handler() {
        use teloxide::dispatching::{Dispatcher, UpdateFilterExt};
        use teloxide::prelude::*;
        let (router, listener) = intake("right".to_string());
        let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<String>();
        let handler = Update::filter_message().endpoint(move |msg: Message| {
            let seen_tx = seen_tx.clone();
            async move {
                seen_tx.send(msg.text().unwrap_or_default().to_string()).unwrap();
                respond(())
            }
        });
        // Telegram, as far as the dispatcher asks it anything: `getMe`.
        let api = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_url = format!("http://{}", api.local_addr().unwrap());
        let me = axum::Router::new().fallback(|| async {
            axum::Json(serde_json::json!({"ok": true, "result": {
                "id": 1, "is_bot": true, "first_name": "Scout", "username": "scout_test_bot",
                "can_join_groups": false, "can_read_all_group_messages": false, "supports_inline_queries": false,
                "can_connect_to_business": false, "has_main_web_app": false
            }}))
        });
        tokio::spawn(async move { axum::serve(api, me).await.unwrap() });
        let bot = teloxide::Bot::new("123:abc").set_api_url(api_url.parse().unwrap());
        let mut dispatcher = Dispatcher::builder(bot, handler).build();
        let shutdown = dispatcher.shutdown_token();
        let run = tokio::spawn(async move {
            dispatcher
                .dispatch_with_listener(listener, teloxide::error_handlers::LoggingErrorHandler::new())
                .await
        });
        assert_eq!(post(&router, Some("right"), UPDATE).await, StatusCode::OK);
        let text = tokio::time::timeout(std::time::Duration::from_secs(5), seen_rx.recv()).await;
        assert_eq!(text.ok().flatten().as_deref(), Some("hello"), "the handler never saw the update");
        if let Ok(done) = shutdown.shutdown() {
            done.await;
        }
        run.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_queue_nobody_takes_from_is_a_stall_and_a_quiet_bot_is_not() {
        let (router, mut listener) = intake_with("right".to_string(), Duration::from_secs(120));
        let health = listener.health();
        // Nothing arriving is fine, however long ago the last one was.
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert_eq!(health.stalled(), None);
        // An update accepted and not yet taken is fine for a while.
        assert_eq!(post(&router, Some("right"), UPDATE).await, StatusCode::OK);
        assert_eq!(health.stalled(), None);
        tokio::time::advance(Duration::from_secs(121)).await;
        assert!(health.stalled().unwrap().starts_with("1 update(s) waiting"), "{:?}", health.stalled());
        // Taken: alive again.
        listener.as_stream().next().await.unwrap().unwrap();
        assert_eq!(health.stalled(), None);
    }
}
