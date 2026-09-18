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

#[derive(Clone)]
struct Intake {
    secret: String,
    queue: mpsc::Sender<Update>,
}

/// The route to mount and the listener to hand the dispatcher.
pub fn intake(secret: String) -> (Router, Listener) {
    let (queue, rx) = mpsc::channel(QUEUE);
    let (token, flag) = mk_stop_token();
    let router = Router::new().route(PATH, post(receive)).with_state(Intake { secret, queue });
    (router, Listener { rx, token, flag, closing: false })
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
    match intake.queue.try_send(update) {
        Ok(()) => StatusCode::OK,
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
                    update = this.rx.recv() => return update.map(|u| (Ok(u), this)),
                }
            }
            this.rx.recv().await.map(|u| (Ok(u), this))
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
}
