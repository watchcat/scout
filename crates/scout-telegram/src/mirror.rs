//! Sending what the browser queued.

use scout_core::core::Core;
use scout_core::mirror::PendingMirror;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use teloxide::prelude::*;
use teloxide::types::InlineKeyboardMarkup;

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
    async fn send(&self, address: &str, body: &str, buttons: Option<InlineKeyboardMarkup>) -> anyhow::Result<()>;
}

/// A queued row and the buttons to hang under it, if any.
pub type Outgoing = (PendingMirror, Option<InlineKeyboardMarkup>);

/// Somewhere to record what happened to a row.
///
/// Separate from the sink because the drain's rule — send in order, stop an
/// account at its first failure — needs no database, and this crate has no
/// way to build a `Core` in a test. Taking both as traits is what makes the
/// rule testable rather than merely written down.
pub trait Ledger {
    async fn sent(&self, id: i64) -> anyhow::Result<()>;
    /// Returns how many attempts the row has now spent, so the caller can
    /// tell a retry from a row it has just given up on.
    async fn failed(&self, id: i64) -> anyhow::Result<i64>;
}

pub struct TelegramSink {
    pub bot: Bot,
}

impl Sink for TelegramSink {
    async fn send(&self, address: &str, body: &str, buttons: Option<InlineKeyboardMarkup>) -> anyhow::Result<()> {
        let chat = address.parse::<i64>()?;
        // The same chunking every other answer gets: Telegram refuses
        // anything past 4096 characters and a price list can exceed it.
        // The buttons go under the last part, where the reader finishes.
        let chunks = crate::text::split_message(body, crate::text::TELEGRAM_LIMIT);
        let last = chunks.len().saturating_sub(1);
        for (i, chunk) in chunks.into_iter().enumerate() {
            let send = self.bot.send_message(ChatId(chat), chunk);
            match (i == last, &buttons) {
                (true, Some(markup)) => send.reply_markup(markup.clone()).await?,
                _ => send.await?,
            };
        }
        Ok(())
    }
}

/// Add and Ignore for a nudge about a mail whose bookings are still
/// pending, and Open <trip> where the Mini App is on. Read at send time
/// rather than queued with the row: a booking decided on the page between
/// the nudge being queued and sent gets no button for a thing already done.
///
/// A web-app button is allowed only in a private chat, which a positive
/// chat id is.
async fn buttons_for(core: &Core, launch: Option<&url::Url>, row: &PendingMirror) -> Option<InlineKeyboardMarkup> {
    let mail_id = crate::arrivals::mail_of_key(&row.turn_key)?;
    let pending = match scout_core::inbox::pending_of_mail(core, row.account_id, mail_id).await {
        Ok(pending) => pending,
        Err(e) => {
            tracing::warn!(error = %e, mail_id, "could not read a nudge's bookings; sending it without buttons");
            return None;
        }
    };
    let private = row.address.parse::<i64>().is_ok_and(|chat| chat > 0);
    let open = match (launch, pending.first().and_then(|a| a.trip_name.as_deref())) {
        (Some(launch), Some(trip)) if private => Some(crate::mini_app::open_trip_button(launch, trip)),
        _ => None,
    };
    crate::arrivals::arrival_markup(&pending, open)
}

pub struct CoreLedger<'a>(pub &'a Core);

impl Ledger for CoreLedger<'_> {
    async fn sent(&self, id: i64) -> anyhow::Result<()> {
        scout_core::mirror::sent(self.0, id).await
    }
    async fn failed(&self, id: i64) -> anyhow::Result<i64> {
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
    due: Vec<Outgoing>,
    sink: &S,
    ledger: &L,
) -> anyhow::Result<()> {
    let mut blocked: HashSet<i64> = HashSet::new();
    for (row, buttons) in due {
        if blocked.contains(&row.account_id) {
            continue;
        }
        match sink.send(&row.address, &row.body, buttons).await {
            Ok(()) => ledger.sent(row.id).await?,
            Err(e) => {
                // Said separately, because "it stays queued" is false on the
                // last attempt and that is the one line a human would read.
                // An abandoned row cannot be recovered: its key stays
                // occupied, so re-enqueueing is a no-op and toggling the
                // mirror off and on will not bring it back.
                let attempts = ledger.failed(row.id).await?;
                if attempts >= scout_core::mirror::ATTEMPTS {
                    tracing::warn!(error = %e, id = row.id, account_id = row.account_id, attempts,
                        "giving up on a mirrored message; it will not be sent and cannot be requeued");
                } else {
                    tracing::warn!(error = %e, id = row.id, account_id = row.account_id, attempts,
                        "a mirrored message did not send; it stays queued");
                }
                blocked.insert(row.account_id);
                continue;
            }
        }
        tokio::time::sleep(PACE).await;
    }
    Ok(())
}

/// Drains whenever something is queued, and every `TICK` regardless.
pub async fn run(bot: Bot, core: Arc<Core>, launch: Option<url::Url>) {
    let sink = TelegramSink { bot };
    loop {
        tokio::select! {
            _ = core.mirror_waiting() => {}
            _ = tokio::time::sleep(TICK) => {}
        }
        match scout_core::mirror::pending(&core, BATCH).await {
            Ok(due) => {
                let mut outgoing = Vec::with_capacity(due.len());
                for row in due {
                    let buttons = buttons_for(&core, launch.as_ref(), &row).await;
                    outgoing.push((row, buttons));
                }
                if let Err(e) = drain(outgoing, &sink, &CoreLedger(&core)).await {
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
        async fn send(&self, _address: &str, body: &str, _buttons: Option<InlineKeyboardMarkup>) -> anyhow::Result<()> {
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
        async fn failed(&self, id: i64) -> anyhow::Result<i64> {
            self.failed.lock().unwrap().push(id);
            Ok(self.failed.lock().unwrap().len() as i64)
        }
    }

    fn row(id: i64, account_id: i64, body: &str) -> PendingMirror {
        PendingMirror {
            id,
            account_id,
            address: "4242".to_string(),
            body: body.to_string(),
            turn_key: format!("turn:{id}"),
        }
    }

    fn plain(rows: Vec<PendingMirror>) -> Vec<Outgoing> {
        rows.into_iter().map(|r| (r, None)).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn a_thread_goes_out_in_order() {
        let (sink, ledger) = (Recorder::default(), Book::default());
        let due = vec![row(1, 7, "> cheapest beans"), row(2, 7, "here are three")];
        drain(plain(due), &sink, &ledger).await.unwrap();
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
        drain(plain(vec![row(1, 7, "first"), row(2, 7, "second")]), &sink, &ledger).await.unwrap();
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
        drain(plain(due), &sink, &ledger).await.unwrap();
        assert_eq!(*sink.sent.lock().unwrap(), vec!["another reader"]);
    }
}
