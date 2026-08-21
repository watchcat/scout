//! When a reminder next comes round.
//!
//! The arithmetic lives here rather than in a channel because a channel that
//! did it itself would be a second opinion on the cadence, and two channels
//! would eventually disagree.

use crate::store::Reminder;
use chrono::{Duration as ChronoDuration, NaiveDate};

/// Step next_due forward by whole intervals until it is after `today`,
/// keeping the cadence anchored to the original date.
///
/// Panics if interval_days < 1.
pub(crate) fn advance_from(next_due: NaiveDate, interval_days: i64, today: NaiveDate) -> NaiveDate {
    assert!(interval_days >= 1, "interval_days must be >= 1");
    let mut d = next_due;
    while d <= today {
        d += ChronoDuration::days(interval_days);
    }
    d
}

/// The date this reminder should next come round, or `None` if it is a row
/// core will not act on.
///
/// The two refusals are what keeps `advance_from`'s assertion from firing:
/// an interval below one has no cadence to advance along, and a date that
/// does not parse has nothing to advance from. Both are logged here, so a
/// row that is skipped says so once whether it was found while listing
/// deliveries or while acknowledging one.
pub(crate) fn next_date(reminder: &Reminder, today: NaiveDate) -> Option<NaiveDate> {
    if reminder.interval_days < 1 {
        tracing::error!(reminder_id = reminder.id, interval_days = reminder.interval_days,
            "invalid interval_days; skipping reminder");
        return None;
    }
    let Ok(parsed) = NaiveDate::parse_from_str(&reminder.next_due, "%Y-%m-%d") else {
        tracing::error!(reminder_id = reminder.id, next_due = %reminder.next_due,
            "unparseable next_due; skipping reminder");
        return None;
    };
    Some(advance_from(parsed, reminder.interval_days, today))
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

    fn reminder(interval_days: i64, next_due: &str) -> Reminder {
        Reminder {
            id: 1,
            account_id: 1,
            channel: "telegram".to_string(),
            address: "1".to_string(),
            item: "detergent".to_string(),
            interval_days,
            next_due: next_due.to_string(),
        }
    }

    #[test]
    fn a_reminder_with_no_cadence_is_refused_rather_than_advanced() {
        assert_eq!(next_date(&reminder(0, "2026-07-22"), date("2026-07-22")), None);
    }

    #[test]
    fn a_reminder_whose_date_does_not_parse_is_refused() {
        assert_eq!(next_date(&reminder(30, "soon"), date("2026-07-22")), None);
    }

    #[test]
    fn a_usable_reminder_gets_the_date_advance_from_would_give_it() {
        assert_eq!(
            next_date(&reminder(30, "2026-07-22"), date("2026-07-22")),
            Some(date("2026-08-21"))
        );
    }
}
