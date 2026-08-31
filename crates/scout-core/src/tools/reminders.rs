use super::purchases::{internal, StoreToolError};
use crate::store::{Reminder, Store};
use chrono::{Duration, NaiveDate};
use rig::tool::Tool;
use scout_api::ReplyTo;
use serde::Deserialize;
use serde_json::json;

/// First due date: anchor on the last matching purchase when known
/// (stepping by interval until the date is in the future), else today + interval.
///
/// Panics if `interval_days < 1` — callers must validate first.
pub fn default_next_due(
    last_purchase: Option<NaiveDate>,
    interval_days: i64,
    today: NaiveDate,
) -> NaiveDate {
    assert!(interval_days >= 1, "interval_days must be >= 1");
    match last_purchase {
        // Stepping from the purchase is the same arithmetic the scheduler
        // does every fifteen minutes, so it is asked for rather than
        // repeated: two copies of a cadence eventually disagree.
        Some(d) => crate::schedule::advance_from(d, interval_days, today),
        None => today + Duration::days(interval_days),
    }
}

#[derive(Deserialize)]
pub struct CreateReminderArgs {
    pub item: String,
    pub interval_days: i64,
    /// YYYY-MM-DD; when omitted it is derived from purchase history.
    pub next_due: Option<String>,
}

pub struct CreateReminderTool {
    pub store: Store,
    pub account_id: i64,
    /// Where a reminder made in this run should be delivered. Carried
    /// rather than looked up, because in a group the address is the group.
    pub reply_to: scout_api::ReplyTo,
}

impl Tool for CreateReminderTool {
    const NAME: &'static str = "create_reminder";
    type Error = StoreToolError;
    type Args = CreateReminderArgs;
    type Output = Reminder;

    fn description(&self) -> String {
        "Create a recurring reorder reminder. ONLY call after the user has \
         explicitly agreed to set up a reminder."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "item": {"type": "string", "description": "what to remind about reordering"},
                "interval_days": {"type": "integer", "description": "cadence in days, >= 1"},
                "next_due": {"type": "string", "description": "first due date YYYY-MM-DD; omit to derive from purchase history"}
            },
            "required": ["item", "interval_days"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        if args.interval_days < 1 {
            return Err(StoreToolError("interval_days must be >= 1".to_string()));
        }
        let next_due = match &args.next_due {
            Some(s) => {
                NaiveDate::parse_from_str(s, "%Y-%m-%d")
                    .map_err(|e| StoreToolError(format!("next_due must be YYYY-MM-DD: {e}")))?;
                s.clone()
            }
            None => {
                let store = self.store.clone();
                let account_id = self.account_id;
                let item = args.item.clone();
                let last = tokio::task::spawn_blocking(move || {
                    store.query_purchases(account_id, Some(&item), 1)
                })
                .await
                .map_err(internal)?
                .map_err(internal)?
                .into_iter()
                .next()
                .and_then(|p| p.purchased_at)
                .and_then(|d| NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok());
                let today = chrono::Local::now().date_naive();
                default_next_due(last, args.interval_days, today).to_string()
            }
        };
        let store = self.store.clone();
        let account_id = self.account_id;
        let (channel, address) = (self.reply_to.channel.clone(), self.reply_to.address.clone());
        tokio::task::spawn_blocking(move || {
            store.create_reminder(
                account_id,
                &channel,
                &address,
                &args.item,
                args.interval_days,
                &next_due,
            )
        })
        .await
        .map_err(internal)?
        .map_err(internal)
    }
}

pub struct ListRemindersTool {
    pub store: Store,
    pub account_id: i64,
}

#[derive(Deserialize)]
pub struct NoArgs {}

impl Tool for ListRemindersTool {
    const NAME: &'static str = "list_reminders";
    type Error = StoreToolError;
    type Args = NoArgs;
    type Output = Vec<Reminder>;

    fn description(&self) -> String {
        "List the user's active reorder reminders with their next due dates.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
        let store = self.store.clone();
        let account_id = self.account_id;
        tokio::task::spawn_blocking(move || store.list_reminders(account_id))
            .await
            .map_err(internal)?
            .map_err(internal)
    }
}

#[derive(Deserialize)]
pub struct CancelReminderArgs {
    pub id: i64,
}

pub struct CancelReminderTool {
    pub store: Store,
    pub account_id: i64,
}

impl Tool for CancelReminderTool {
    const NAME: &'static str = "cancel_reminder";
    type Error = StoreToolError;
    type Args = CancelReminderArgs;
    type Output = String;

    fn description(&self) -> String {
        "Cancel one of the user's reminders by id (see list_reminders).".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {"id": {"type": "integer", "description": "reminder id"}},
            "required": ["id"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let store = self.store.clone();
        let account_id = self.account_id;
        let cancelled = tokio::task::spawn_blocking(move || store.cancel_reminder(account_id, args.id))
            .await
            .map_err(internal)?
            .map_err(internal)?;
        Ok(if cancelled {
            "reminder cancelled".to_string()
        } else {
            "no active reminder with that id".to_string()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn date(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn default_next_due_from_last_purchase_steps_past_today() {
        // last bought Jan 1, every 30 days, today Jul 22 → first future step
        assert_eq!(
            default_next_due(Some(date("2026-01-01")), 30, date("2026-07-22")),
            date("2026-07-30")
        );
    }

    #[test]
    fn default_next_due_without_history_is_today_plus_interval() {
        assert_eq!(
            default_next_due(None, 30, date("2026-07-22")),
            date("2026-08-21")
        );
    }

    #[test]
    #[should_panic]
    fn default_next_due_panics_on_non_positive_interval() {
        default_next_due(None, 0, date("2026-07-22"));
    }

    fn setup() -> (Store, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("t.duckdb")).unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn create_list_cancel_via_tools() {
        let (store, _d) = setup();
        let create = CreateReminderTool { store: store.clone(), account_id: 1, reply_to: ReplyTo::telegram(99) };
        let r = create
            .call(CreateReminderArgs {
                item: "coffee".into(),
                interval_days: 30,
                next_due: Some("2026-08-01".into()),
            })
            .await
            .unwrap();
        assert_eq!(r.address, "99");
        assert_eq!(r.next_due, "2026-08-01");

        let list = ListRemindersTool { store: store.clone(), account_id: 1 };
        assert_eq!(list.call(NoArgs {}).await.unwrap().len(), 1);

        let cancel = CancelReminderTool { store, account_id: 1 };
        assert_eq!(cancel.call(CancelReminderArgs { id: r.id }).await.unwrap(), "reminder cancelled");
    }

    #[tokio::test]
    async fn create_rejects_bad_interval_and_bad_date() {
        let (store, _d) = setup();
        let create = CreateReminderTool { store, account_id: 1, reply_to: ReplyTo::telegram(99) };
        assert!(create
            .call(CreateReminderArgs { item: "x".into(), interval_days: 0, next_due: None })
            .await
            .is_err());
        assert!(create
            .call(CreateReminderArgs {
                item: "x".into(),
                interval_days: 30,
                next_due: Some("next tuesday".into()),
            })
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_reminder_made_in_a_group_is_addressed_to_the_group() {
        // Why `ReplyTo` is carried for the run rather than resolved from
        // the account's `deliveries` row at delivery time: in a group,
        // the address is the group. Resolving per-account would send this
        // wherever its owner last spoke instead.
        let (store, _d) = setup();
        let group = ReplyTo::telegram(-100_123);
        let create = CreateReminderTool {
            store: store.clone(),
            account_id: 1,
            reply_to: group.clone(),
        };

        let r = create
            .call(CreateReminderArgs {
                item: "team coffee".into(),
                interval_days: 30,
                next_due: Some("2026-09-01".into()),
            })
            .await
            .unwrap();

        assert_eq!(r.channel, "telegram");
        assert_eq!(r.address, "-100123", "a group reminder lost its group");
    }
}
