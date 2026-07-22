use crate::store::Store;
use anyhow::Result;
use chrono::{Duration as ChronoDuration, NaiveDate};
use std::time::Duration;
use teloxide::prelude::*;

const TICK: Duration = Duration::from_secs(15 * 60);

/// Step next_due forward by whole intervals until it is after `today`,
/// keeping the cadence anchored to the original date.
///
/// Panics if interval_days < 1.
pub fn advance_from(next_due: NaiveDate, interval_days: i64, today: NaiveDate) -> NaiveDate {
    assert!(interval_days >= 1, "interval_days must be >= 1");
    let mut d = next_due;
    while d <= today {
        d += ChronoDuration::days(interval_days);
    }
    d
}

/// Background loop: every 15 minutes deliver due reminders. A failed send
/// leaves next_due unchanged so the next tick retries it.
pub async fn run(bot: Bot, store: Store) {
    let mut interval = tokio::time::interval(TICK);
    loop {
        interval.tick().await;
        let today = chrono::Local::now().date_naive();
        if let Err(e) = tick(&bot, &store, today).await {
            tracing::error!(error = %e, "reminder tick failed");
        }
    }
}

async fn tick(bot: &Bot, store: &Store, today: NaiveDate) -> Result<()> {
    let due = {
        let store = store.clone();
        let today_s = today.to_string();
        tokio::task::spawn_blocking(move || store.due_reminders(&today_s)).await??
    };
    for reminder in due {
        let text = format!(
            "⏰ Time to reorder {} — want me to search for deals?",
            reminder.item
        );
        match bot.send_message(ChatId(reminder.chat_id), text).await {
            Ok(_) => {
                let next_due = advance_from(
                    NaiveDate::parse_from_str(&reminder.next_due, "%Y-%m-%d")?,
                    reminder.interval_days,
                    today,
                );
                let store = store.clone();
                let next_s = next_due.to_string();
                tokio::task::spawn_blocking(move || store.set_next_due(reminder.id, &next_s))
                    .await??;
            }
            Err(e) => {
                tracing::warn!(reminder_id = reminder.id, error = %e,
                    "reminder send failed; will retry next tick");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn due_today_advances_one_interval() {
        assert_eq!(
            advance_from(date("2026-07-22"), 30, date("2026-07-22")),
            date("2026-08-21")
        );
    }

    #[test]
    fn long_overdue_steps_to_first_future_date_keeping_cadence() {
        assert_eq!(
            advance_from(date("2026-01-01"), 30, date("2026-07-22")),
            date("2026-07-30")
        );
    }

    #[test]
    fn future_date_is_unchanged() {
        assert_eq!(
            advance_from(date("2026-09-01"), 30, date("2026-07-22")),
            date("2026-09-01")
        );
    }

    #[test]
    #[should_panic]
    fn advance_from_panics_on_non_positive_interval() {
        advance_from(date("2026-07-22"), 0, date("2026-07-22"));
    }
}
