use anyhow::Result;
use duckdb::{params, Connection, Row};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

const MIGRATIONS: &str = r#"
CREATE SEQUENCE IF NOT EXISTS purchases_id_seq;
CREATE TABLE IF NOT EXISTS purchases (
    id BIGINT PRIMARY KEY DEFAULT nextval('purchases_id_seq'),
    user_id BIGINT NOT NULL,
    item TEXT NOT NULL,
    store TEXT NOT NULL,
    url TEXT,
    price DOUBLE,
    currency TEXT,
    notes TEXT,
    purchased_at TEXT,
    recorded_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE SEQUENCE IF NOT EXISTS reminders_id_seq;
CREATE TABLE IF NOT EXISTS reminders (
    id BIGINT PRIMARY KEY DEFAULT nextval('reminders_id_seq'),
    user_id BIGINT NOT NULL,
    chat_id BIGINT NOT NULL,
    item TEXT NOT NULL,
    interval_days BIGINT NOT NULL,
    next_due TEXT NOT NULL,
    active BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE TABLE IF NOT EXISTS user_facts (
    user_id BIGINT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (user_id, key)
);
CREATE TABLE IF NOT EXISTS request_log (
    user_id BIGINT NOT NULL,
    kind TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
-- Telegram display names, refreshed on every request so /stat can label a
-- user id with something readable. Names change; the id is the identity.
CREATE TABLE IF NOT EXISTS users (
    user_id BIGINT PRIMARY KEY,
    display_name TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
"#;

/// A purchase as the agent sees it. `purchased_at` is an ISO `YYYY-MM-DD`
/// string; TEXT keeps date handling trivial and sorts chronologically.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Purchase {
    pub id: i64,
    pub item: String,
    pub store: String,
    pub url: Option<String>,
    pub price: Option<f64>,
    pub currency: Option<String>,
    pub notes: Option<String>,
    pub purchased_at: Option<String>,
}

/// Also serves as the `record_purchase` tool's Args.
#[derive(Debug, Clone, Deserialize)]
pub struct NewPurchase {
    pub item: String,
    pub store: String,
    pub url: Option<String>,
    pub price: Option<f64>,
    pub currency: Option<String>,
    pub notes: Option<String>,
    pub purchased_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reminder {
    pub id: i64,
    #[serde(skip)]
    pub user_id: i64,
    #[serde(skip)]
    pub chat_id: i64,
    pub item: String,
    pub interval_days: i64,
    pub next_due: String, // YYYY-MM-DD
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref())?;
        conn.execute_batch(MIGRATIONS)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn record_purchase(&self, user_id: i64, p: NewPurchase) -> Result<Purchase> {
        let conn = self.conn.lock().unwrap();
        let id: i64 = conn.query_row(
            "INSERT INTO purchases (user_id, item, store, url, price, currency, notes, purchased_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
            params![user_id, p.item, p.store, p.url, p.price, p.currency, p.notes, p.purchased_at],
            |row| row.get(0),
        )?;
        Ok(Purchase {
            id,
            item: p.item,
            store: p.store,
            url: p.url,
            price: p.price,
            currency: p.currency,
            notes: p.notes,
            purchased_at: p.purchased_at,
        })
    }

    /// Case-insensitive substring match on item/store/notes, newest first.
    pub fn query_purchases(
        &self,
        user_id: i64,
        term: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Purchase>> {
        const SELECT: &str =
            "SELECT id, item, store, url, price, currency, notes, purchased_at FROM purchases";
        const ORDER: &str = "ORDER BY coalesce(purchased_at, '') DESC, id DESC LIMIT ?";
        let conn = self.conn.lock().unwrap();
        let mut out = Vec::new();
        match term {
            Some(t) => {
                let like = format!("%{}%", t.to_lowercase());
                let sql = format!(
                    "{SELECT} WHERE user_id = ? AND (lower(item) LIKE ? \
                     OR lower(store) LIKE ? OR lower(coalesce(notes, '')) LIKE ?) {ORDER}"
                );
                let mut stmt = conn.prepare(&sql)?;
                let rows =
                    stmt.query_map(params![user_id, like, like, like, limit as i64], row_to_purchase)?;
                for row in rows {
                    out.push(row?);
                }
            }
            None => {
                let sql = format!("{SELECT} WHERE user_id = ? {ORDER}");
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params![user_id, limit as i64], row_to_purchase)?;
                for row in rows {
                    out.push(row?);
                }
            }
        }
        Ok(out)
    }

    pub fn create_reminder(
        &self,
        user_id: i64,
        chat_id: i64,
        item: &str,
        interval_days: i64,
        next_due: &str,
    ) -> Result<Reminder> {
        let conn = self.conn.lock().unwrap();
        let id: i64 = conn.query_row(
            "INSERT INTO reminders (user_id, chat_id, item, interval_days, next_due)
             VALUES (?, ?, ?, ?, ?) RETURNING id",
            params![user_id, chat_id, item, interval_days, next_due],
            |row| row.get(0),
        )?;
        Ok(Reminder {
            id,
            user_id,
            chat_id,
            item: item.to_string(),
            interval_days,
            next_due: next_due.to_string(),
        })
    }

    /// Active reminders for one user, soonest first.
    pub fn list_reminders(&self, user_id: i64) -> Result<Vec<Reminder>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, user_id, chat_id, item, interval_days, next_due FROM reminders
             WHERE user_id = ? AND active ORDER BY next_due ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![user_id], row_to_reminder)?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Returns true if an active reminder belonging to this user was cancelled.
    pub fn cancel_reminder(&self, user_id: i64, id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE reminders SET active = false WHERE id = ? AND user_id = ? AND active",
            params![id, user_id],
        )?;
        Ok(n > 0)
    }

    /// All users' active reminders with next_due <= today (ISO date string).
    pub fn due_reminders(&self, today: &str) -> Result<Vec<Reminder>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, user_id, chat_id, item, interval_days, next_due FROM reminders
             WHERE active AND next_due <= ? ORDER BY next_due ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![today], row_to_reminder)?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Internal: id must come from a trusted source (the scheduler) — no owner check.
    pub fn set_next_due(&self, id: i64, next_due: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE reminders SET next_due = ? WHERE id = ?",
            params![next_due, id],
        )?;
        Ok(())
    }

    /// Insert or overwrite one user-profile fact.
    pub fn upsert_fact(&self, user_id: i64, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO user_facts (user_id, key, value) VALUES (?, ?, ?)
             ON CONFLICT (user_id, key)
             DO UPDATE SET value = excluded.value, updated_at = now()",
            params![user_id, key, value],
        )?;
        Ok(())
    }

    /// One user's profile facts as (key, value), sorted by key.
    pub fn list_facts(&self, user_id: i64) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT key, value FROM user_facts WHERE user_id = ? ORDER BY key ASC")?;
        let rows = stmt.query_map(params![user_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Returns true if the fact existed and was removed.
    pub fn forget_fact(&self, user_id: i64, key: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM user_facts WHERE user_id = ? AND key = ?",
            params![user_id, key],
        )?;
        Ok(n > 0)
    }

    /// Record one handled request for usage statistics. `display_name` is
    /// the sender's current Telegram name, refreshed on every request so
    /// `/stat` can show something other than a bare id.
    pub fn log_request(&self, user_id: i64, kind: &str, display_name: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO request_log (user_id, kind) VALUES (?, ?)",
            params![user_id, kind],
        )?;
        if let Some(name) = display_name.map(str::trim).filter(|n| !n.is_empty()) {
            conn.execute(
                "INSERT INTO users (user_id, display_name) VALUES (?, ?)
                 ON CONFLICT (user_id)
                 DO UPDATE SET display_name = excluded.display_name, updated_at = now()",
                params![user_id, name],
            )?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn log_request_at(&self, user_id: i64, kind: &str, at: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO request_log (user_id, kind, created_at) VALUES (?, ?, CAST(? AS TIMESTAMP))",
            params![user_id, kind, at],
        )?;
        Ok(())
    }

    /// Per-day request counts scoped to a single user, as
    /// (user_id, day "YYYY-MM-DD", count) at or after `cutoff`
    /// ("YYYY-MM-DD 00:00:00"). This is what non-admin `/stat` callers get,
    /// so they only ever see their own volume however many users share the
    /// bot.
    pub fn usage_stats_for(&self, cutoff: &str, user_id: i64) -> Result<Vec<(i64, String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT user_id, strftime(created_at, '%Y-%m-%d') AS day, count(*)
             FROM request_log WHERE user_id = ? AND created_at >= CAST(? AS TIMESTAMP)
             GROUP BY user_id, day ORDER BY day ASC, user_id ASC",
        )?;
        let rows = stmt.query_map(params![user_id, cutoff], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// The same shape across every user. Only reachable from `/stat` when
    /// the caller is in `Config::admin_user_ids` — the callers of this
    /// method are the whole access-control surface for cross-user data.
    pub fn usage_stats_all(&self, cutoff: &str) -> Result<Vec<(i64, String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT user_id, strftime(created_at, '%Y-%m-%d') AS day, count(*)
             FROM request_log WHERE created_at >= CAST(? AS TIMESTAMP)
             GROUP BY user_id, day ORDER BY day ASC, user_id ASC",
        )?;
        let rows = stmt.query_map(params![cutoff], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Last-seen display name per user id. Small enough to read whole —
    /// one row per person who has ever messaged the bot.
    pub fn display_names(&self) -> Result<BTreeMap<i64, String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT user_id, display_name FROM users")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }
}

fn row_to_purchase(row: &Row) -> duckdb::Result<Purchase> {
    Ok(Purchase {
        id: row.get(0)?,
        item: row.get(1)?,
        store: row.get(2)?,
        url: row.get(3)?,
        price: row.get(4)?,
        currency: row.get(5)?,
        notes: row.get(6)?,
        purchased_at: row.get(7)?,
    })
}

fn row_to_reminder(row: &Row) -> duckdb::Result<Reminder> {
    Ok(Reminder {
        id: row.get(0)?,
        user_id: row.get(1)?,
        chat_id: row.get(2)?,
        item: row.get(3)?,
        interval_days: row.get(4)?,
        next_due: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    pub(crate) fn test_store() -> (Store, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("test.duckdb")).unwrap();
        (store, dir)
    }

    fn new_purchase(item: &str, store: &str, purchased_at: Option<&str>) -> NewPurchase {
        NewPurchase {
            item: item.to_string(),
            store: store.to_string(),
            url: None,
            price: Some(9.99),
            currency: Some("EUR".to_string()),
            notes: None,
            purchased_at: purchased_at.map(str::to_string),
        }
    }

    #[test]
    fn record_and_query_roundtrip() {
        let (s, _d) = test_store();
        let p = s
            .record_purchase(1, new_purchase("Lavazza coffee beans", "Amazon", Some("2026-06-28")))
            .unwrap();
        assert_eq!(p.id, 1);
        assert_eq!(p.item, "Lavazza coffee beans");

        let found = s.query_purchases(1, None, 10).unwrap();
        assert_eq!(found, vec![p]);
    }

    #[test]
    fn queries_are_scoped_per_user() {
        let (s, _d) = test_store();
        s.record_purchase(1, new_purchase("keyboard", "eBay", None)).unwrap();
        s.record_purchase(2, new_purchase("mouse", "eBay", None)).unwrap();

        let mine = s.query_purchases(1, None, 10).unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].item, "keyboard");
    }

    #[test]
    fn substring_match_is_case_insensitive_over_item_store_notes() {
        let (s, _d) = test_store();
        s.record_purchase(1, new_purchase("Coffee beans", "Amazon", None)).unwrap();
        s.record_purchase(1, new_purchase("Tea", "CoffeeCorner", None)).unwrap();
        let mut with_notes = new_purchase("Filters", "Bol", None);
        with_notes.notes = Some("for the coffee machine".to_string());
        s.record_purchase(1, with_notes).unwrap();
        s.record_purchase(1, new_purchase("Socks", "Zalando", None)).unwrap();

        let found = s.query_purchases(1, Some("COFFEE"), 10).unwrap();
        assert_eq!(found.len(), 3);
    }

    #[test]
    fn newest_purchase_first_and_limit_respected() {
        let (s, _d) = test_store();
        s.record_purchase(1, new_purchase("old", "A", Some("2026-01-01"))).unwrap();
        s.record_purchase(1, new_purchase("new", "A", Some("2026-06-01"))).unwrap();
        s.record_purchase(1, new_purchase("undated", "A", None)).unwrap();

        let found = s.query_purchases(1, None, 2).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].item, "new");
        assert_eq!(found[1].item, "old");
    }

    #[test]
    fn create_list_cancel_reminder() {
        let (s, _d) = test_store();
        let r = s.create_reminder(1, 10, "coffee", 30, "2026-08-01").unwrap();
        assert_eq!(r.id, 1);

        let listed = s.list_reminders(1).unwrap();
        assert_eq!(listed, vec![r.clone()]);
        assert!(s.list_reminders(2).unwrap().is_empty());

        assert!(s.cancel_reminder(1, r.id).unwrap());
        assert!(s.list_reminders(1).unwrap().is_empty());
        // second cancel is a no-op
        assert!(!s.cancel_reminder(1, r.id).unwrap());
    }

    #[test]
    fn cancel_is_scoped_to_owner() {
        let (s, _d) = test_store();
        let r = s.create_reminder(1, 10, "coffee", 30, "2026-08-01").unwrap();
        assert!(!s.cancel_reminder(2, r.id).unwrap());
        assert_eq!(s.list_reminders(1).unwrap().len(), 1);
    }

    #[test]
    fn due_reminders_selects_past_and_today_only() {
        let (s, _d) = test_store();
        s.create_reminder(1, 10, "overdue", 30, "2026-07-01").unwrap();
        s.create_reminder(1, 10, "today", 30, "2026-07-22").unwrap();
        s.create_reminder(1, 10, "future", 30, "2026-09-01").unwrap();
        let cancelled = s.create_reminder(1, 10, "cancelled", 30, "2026-07-01").unwrap();
        s.cancel_reminder(1, cancelled.id).unwrap();
        s.create_reminder(2, 20, "other-user", 30, "2026-07-02").unwrap();

        let due = s.due_reminders("2026-07-22").unwrap();
        let items: Vec<_> = due.iter().map(|r| r.item.as_str()).collect();
        assert_eq!(items, vec!["overdue", "other-user", "today"]);
    }

    #[test]
    fn set_next_due_updates() {
        let (s, _d) = test_store();
        let r = s.create_reminder(1, 10, "coffee", 30, "2026-07-01").unwrap();
        s.set_next_due(r.id, "2026-08-01").unwrap();
        assert!(s.due_reminders("2026-07-22").unwrap().is_empty());
        assert_eq!(s.list_reminders(1).unwrap()[0].next_due, "2026-08-01");
    }

    #[test]
    fn facts_upsert_overwrites_and_lists_sorted() {
        let (s, _d) = test_store();
        s.upsert_fact(1, "shoe_size", "43").unwrap();
        s.upsert_fact(1, "delivery_country", "NL").unwrap();
        s.upsert_fact(1, "shoe_size", "44").unwrap();

        assert_eq!(
            s.list_facts(1).unwrap(),
            vec![
                ("delivery_country".to_string(), "NL".to_string()),
                ("shoe_size".to_string(), "44".to_string()),
            ]
        );
    }

    #[test]
    fn facts_are_scoped_per_user() {
        let (s, _d) = test_store();
        s.upsert_fact(1, "delivery_country", "NL").unwrap();
        assert!(s.list_facts(2).unwrap().is_empty());
        assert!(!s.forget_fact(2, "delivery_country").unwrap());
        assert_eq!(s.list_facts(1).unwrap().len(), 1);
    }

    #[test]
    fn usage_stats_for_is_scoped_to_one_user() {
        // /stat pulls from this method so a per-user query never sees
        // anyone else's request count.
        let (s, _d) = test_store();
        s.log_request_at(1, "text", "2026-07-25 10:00:00").unwrap();
        s.log_request_at(1, "text", "2026-07-25 11:00:00").unwrap();
        s.log_request_at(2, "photo", "2026-07-25 12:00:00").unwrap();
        s.log_request_at(1, "text", "2026-07-20 09:00:00").unwrap(); // before cutoff

        let mine = s.usage_stats_for("2026-07-25 00:00:00", 1).unwrap();
        assert_eq!(
            mine,
            vec![(1, "2026-07-25".to_string(), 2)],
            "user 1 should see only their own rows"
        );

        let theirs = s.usage_stats_for("2026-07-25 00:00:00", 2).unwrap();
        assert_eq!(
            theirs,
            vec![(2, "2026-07-25".to_string(), 1)],
            "user 2 must not see user 1's counts"
        );

        let empty = s.usage_stats_for("2026-07-25 00:00:00", 99).unwrap();
        assert!(empty.is_empty(), "unknown user sees nothing");
    }

    #[test]
    fn usage_stats_all_spans_every_user() {
        // The admin view. Same shape as the per-user query so /stat can
        // render either without knowing which it got.
        let (s, _d) = test_store();
        s.log_request_at(1, "text", "2026-07-25 10:00:00").unwrap();
        s.log_request_at(1, "text", "2026-07-25 11:00:00").unwrap();
        s.log_request_at(2, "photo", "2026-07-25 12:00:00").unwrap();
        s.log_request_at(1, "text", "2026-07-26 09:00:00").unwrap();
        s.log_request_at(1, "text", "2026-07-20 09:00:00").unwrap(); // before cutoff

        let rows = s.usage_stats_all("2026-07-25 00:00:00").unwrap();
        assert_eq!(
            rows,
            vec![
                (1, "2026-07-25".to_string(), 2),
                (2, "2026-07-25".to_string(), 1),
                (1, "2026-07-26".to_string(), 1),
            ]
        );

        // live logging path writes with defaults and lands in stats
        s.log_request(3, "reaction", None).unwrap();
        let rows = s.usage_stats_all("2000-01-01 00:00:00").unwrap();
        assert!(rows.iter().any(|(u, _, _)| *u == 3));
    }

    #[test]
    fn display_names_track_the_latest_seen_name() {
        let (s, _d) = test_store();
        s.log_request(1, "text", Some("@alice")).unwrap();
        s.log_request(2, "text", Some("Bob Jansen")).unwrap();
        // A user with no name attached is logged but stays unnamed, so
        // /stat falls back to the bare id.
        s.log_request(3, "text", None).unwrap();
        // Blank names are not a name.
        s.log_request(4, "text", Some("   ")).unwrap();

        let names = s.display_names().unwrap();
        assert_eq!(names.get(&1).map(String::as_str), Some("@alice"));
        assert_eq!(names.get(&2).map(String::as_str), Some("Bob Jansen"));
        assert_eq!(names.get(&3), None);
        assert_eq!(names.get(&4), None);

        // Renaming overwrites rather than accumulating rows.
        s.log_request(1, "text", Some("@alice_new")).unwrap();
        let names = s.display_names().unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names.get(&1).map(String::as_str), Some("@alice_new"));
    }

    #[test]
    fn forget_fact_removes_and_reports() {
        let (s, _d) = test_store();
        s.upsert_fact(1, "budget_style", "prefers cheap used gear").unwrap();
        assert!(s.forget_fact(1, "budget_style").unwrap());
        assert!(!s.forget_fact(1, "budget_style").unwrap());
        assert!(s.list_facts(1).unwrap().is_empty());
    }
}
