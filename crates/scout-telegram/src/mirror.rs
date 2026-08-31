//! Sending what the browser queued.

use scout_core::core::Core;
use scout_core::mirror::PendingMirror;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use teloxide::prelude::*;

/// How long to leave between messages to one chat.
///
/// Telegram's sustained ceiling for a private chat is about one message a
/// second, and a backfill is the one thing here that sends a burst. A
/// twenty-message thread takes twenty seconds, which nobody notices, and
/// pacing it is cheaper than earning a `RetryAfter` — one was observed at
/// 238 seconds on this bot.
const PACE: Duration = Duration::from_millis(1100);

/// How many rows one pass will move. A ceiling rather than a target: a
/// backfill is bounded by `HISTORY_CAP` anyway.
const BATCH: usize = 64;

/// How often to look even when nothing has said to.
///
/// The notify is what makes delivery prompt; this is what makes a missed
/// signal a delay rather than a lost mirror.
const TICK: Duration = Duration::from_secs(60);

/// Somewhere for a mirrored message to go.
///
/// A trait so the drain can be tested with no bot token and no network,
/// exactly as `progress::Renderer` is.
pub trait Sink {
    async fn send(&self, address: &str, body: &str) -> anyhow::Result<()>;
}

/// Somewhere to record what happened to a row.
///
/// Separate from the sink because the drain's rule — send in order, stop an
/// account at its first failure — needs no database, and this crate has no
/// way to build a `Core` in a test. Taking both as traits is what makes the
/// rule testable rather than merely written down.
pub trait Ledger {
    async fn sent(&self, id: i64) -> anyhow::Result<()>;
    async fn failed(&self, id: i64) -> anyhow::Result<()>;
}

pub struct TelegramSink {
    pub bot: Bot,
}

impl Sink for TelegramSink {
    async fn send(&self, address: &str, body: &str) -> anyhow::Result<()> {
        let chat = address.parse::<i64>()?;
        // The same chunking every other answer gets: Telegram refuses
        // anything past 4096 characters and a price list can exceed it.
        for chunk in crate::text::split_message(body, crate::text::TELEGRAM_LIMIT) {
            self.bot.send_message(ChatId(chat), chunk).await?;
        }
        Ok(())
    }
}

pub struct CoreLedger<'a>(pub &'a Core);

impl Ledger for CoreLedger<'_> {
    async fn sent(&self, id: i64) -> anyhow::Result<()> {
        scout_core::mirror::sent(self.0, id).await
    }
    async fn failed(&self, id: i64) -> anyhow::Result<()> {
        scout_core::mirror::failed(self.0, id).await
    }
}

/// Sends what is waiting, in the order given, and stops an account at its
/// first failure.
///
/// Stopping rather than skipping is the whole point: the rows are one
/// conversation in order, and a later turn arriving before an earlier one
/// reads as nonsense. The stop is per account, so one reader who has
/// blocked the bot cannot freeze everybody else's thread behind them.
pub async fn drain<S: Sink, L: Ledger>(
    due: Vec<PendingMirror>,
    sink: &S,
    ledger: &L,
) -> anyhow::Result<()> {
    let mut blocked: HashSet<i64> = HashSet::new();
    for row in due {
        if blocked.contains(&row.account_id) {
            continue;
        }
        match sink.send(&row.address, &row.body).await {
            Ok(()) => ledger.sent(row.id).await?,
            Err(e) => {
                tracing::warn!(error = %e, id = row.id, account_id = row.account_id,
                    "a mirrored message did not send; it stays queued");
                ledger.failed(row.id).await?;
                blocked.insert(row.account_id);
                continue;
            }
        }
        tokio::time::sleep(PACE).await;
    }
    Ok(())
}

/// Drains whenever something is queued, and every `TICK` regardless.
pub async fn run(bot: Bot, core: Arc<Core>) {
    let sink = TelegramSink { bot };
    loop {
        tokio::select! {
            _ = core.mirror_waiting() => {}
            _ = tokio::time::sleep(TICK) => {}
        }
        match scout_core::mirror::pending(&core, BATCH).await {
            Ok(due) => {
                if let Err(e) = drain(due, &sink, &CoreLedger(&core)).await {
                    tracing::error!(error = %e, "the mirror drain failed");
                }
            }
            Err(e) => tracing::error!(error = %e, "could not read the mirror queue"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A sink that writes to a list instead of to Telegram — the same trick
    /// `progress.rs` uses for `Renderer`, and for the same reason: no token,
    /// no network, and a test that can assert on order.
    #[derive(Default)]
    struct Recorder {
        sent: Mutex<Vec<String>>,
        fail_on: Option<String>,
    }

    impl Sink for Recorder {
        async fn send(&self, _address: &str, body: &str) -> anyhow::Result<()> {
            if self.fail_on.as_deref() == Some(body) {
                anyhow::bail!("telegram said no");
            }
            self.sent.lock().unwrap().push(body.to_string());
            Ok(())
        }
    }

    #[derive(Default)]
    struct Book {
        sent: Mutex<Vec<i64>>,
        failed: Mutex<Vec<i64>>,
    }

    impl Ledger for Book {
        async fn sent(&self, id: i64) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push(id);
            Ok(())
        }
        async fn failed(&self, id: i64) -> anyhow::Result<()> {
            self.failed.lock().unwrap().push(id);
            Ok(())
        }
    }

    fn row(id: i64, account_id: i64, body: &str) -> PendingMirror {
        PendingMirror {
            id,
            account_id,
            address: "4242".to_string(),
            body: body.to_string(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_thread_goes_out_in_order() {
        let (sink, ledger) = (Recorder::default(), Book::default());
        let due = vec![row(1, 7, "> cheapest beans"), row(2, 7, "here are three")];
        drain(due, &sink, &ledger).await.unwrap();
        assert_eq!(*sink.sent.lock().unwrap(), vec!["> cheapest beans", "here are three"]);
        assert_eq!(*ledger.sent.lock().unwrap(), vec![1, 2]);
        assert!(ledger.failed.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failure_stops_that_account_rather_than_racing_past_it() {
        // Skipping a failed row would land a later turn ahead of an earlier
        // one. A thread out of order is worse than a thread that is late.
        let sink = Recorder { sent: Mutex::new(Vec::new()), fail_on: Some("first".to_string()) };
        let ledger = Book::default();
        drain(vec![row(1, 7, "first"), row(2, 7, "second")], &sink, &ledger).await.unwrap();
        assert!(sink.sent.lock().unwrap().is_empty(), "sent the second before the first landed");
        assert_eq!(*ledger.failed.lock().unwrap(), vec![1]);
        assert!(ledger.sent.lock().unwrap().is_empty(), "marked something sent that never went");
    }

    #[tokio::test(start_paused = true)]
    async fn one_blocked_reader_does_not_hold_up_another() {
        // The stop is per account, not per queue: someone who has blocked
        // the bot must not freeze everybody else's thread behind them.
        let sink = Recorder { sent: Mutex::new(Vec::new()), fail_on: Some("blocked".to_string()) };
        let ledger = Book::default();
        let due = vec![row(1, 7, "blocked"), row(2, 7, "also seven"), row(3, 8, "another reader")];
        drain(due, &sink, &ledger).await.unwrap();
        assert_eq!(*sink.sent.lock().unwrap(), vec!["another reader"]);
    }
}
