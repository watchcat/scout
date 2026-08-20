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
-- Where each person last spoke to the bot, so an announcement can reach
-- them. A separate table rather than a column on `users`: that one already
-- holds live data, and adding to it means altering it in place.
CREATE TABLE IF NOT EXISTS user_chats (
    user_id BIGINT PRIMARY KEY,
    chat_id BIGINT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
-- Admission. `ALLOWED_TELEGRAM_USER_IDS` stays the founder list; these three
-- tables are where growth lives — see the invite-links design doc.
--
-- A round is a named code with a capacity, shared as a t.me deep link.
CREATE TABLE IF NOT EXISTS invite_rounds (
    code       TEXT PRIMARY KEY,
    capacity   BIGINT NOT NULL,
    open       BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
-- One row per person admitted, ever. `user_id` is the key, so a person
-- belongs to one round; `revoked_at` set means removed, and the row stays
-- put so the seat is not handed back.
CREATE TABLE IF NOT EXISTS members (
    user_id    BIGINT PRIMARY KEY,
    code       TEXT NOT NULL,
    joined_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    revoked_at TIMESTAMP
);
-- People a full or unknown round turned away. `chat_id` is stored because
-- reaching them later is the whole point, and the START they pressed is the
-- permission to do it.
CREATE TABLE IF NOT EXISTS waitlist (
    user_id    BIGINT PRIMARY KEY,
    chat_id    BIGINT NOT NULL,
    code       TEXT NOT NULL,
    seen_at    TIMESTAMP NOT NULL DEFAULT current_timestamp,
    invited_at TIMESTAMP
);
-- A named plan. The itinerary is durable; prices are not, so nothing here
-- holds an offer id — see the trips design doc.
CREATE SEQUENCE IF NOT EXISTS trips_id_seq;
CREATE TABLE IF NOT EXISTS trips (
    id          BIGINT PRIMARY KEY DEFAULT nextval('trips_id_seq'),
    user_id     BIGINT NOT NULL,
    name        TEXT NOT NULL,
    -- lowercased `name`: the trip is addressed by what the traveller calls
    -- it, and "September" and "september" are the same trip.
    name_key    TEXT NOT NULL,
    adults      BIGINT NOT NULL DEFAULT 1,
    cabin_class TEXT,
    status      TEXT NOT NULL DEFAULT 'planning',
    created_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    UNIQUE (user_id, name_key)
);
-- Where and when. This is all that gets re-searched.
CREATE TABLE IF NOT EXISTS trip_segments (
    trip_id        BIGINT NOT NULL,
    position       BIGINT NOT NULL,
    origin         TEXT NOT NULL,
    destination    TEXT NOT NULL,
    departure_date TEXT NOT NULL,
    -- Hands out candidate numbers and never takes one back. Deriving the next
    -- number from max(candidate) over live rows recycles it as soon as the
    -- highest is dropped, and a traveller who was shown "option 2" would
    -- then be given a different flight under the same name.
    next_candidate BIGINT NOT NULL DEFAULT 1,
    PRIMARY KEY (trip_id, position)
);
-- The options on a segment. Several may sit here undecided; at most one
-- carries `chosen`, which is enforced in Rust because `false` repeats.
CREATE TABLE IF NOT EXISTS segment_candidates (
    trip_id            BIGINT NOT NULL,
    position           BIGINT NOT NULL,
    candidate          BIGINT NOT NULL,
    chosen             BOOLEAN NOT NULL DEFAULT false,
    airline            TEXT NOT NULL,
    flight_numbers     TEXT NOT NULL,
    itinerary          TEXT NOT NULL,
    departing_at_local TEXT,
    arriving_at_local  TEXT,
    duration_minutes   BIGINT,
    quoted_price       DOUBLE,
    quoted_currency    TEXT,
    quoted_at          TIMESTAMP,
    source             TEXT,
    PRIMARY KEY (trip_id, position, candidate)
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

/// A named plan. `id` is not serialised: it is noise to the model, and
/// exposing it invites addressing a trip by something the traveller never
/// said.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Trip {
    #[serde(skip)]
    pub id: i64,
    pub name: String,
    pub adults: i64,
    pub cabin_class: Option<String>,
    /// `planning` or `finalised`. Any edit that changes what would be
    /// priced — a segment, its options, the passenger count or the cabin —
    /// returns it to `planning`: the prices it was finalised at stopped
    /// describing the trip when the trip stopped being that trip.
    pub status: String,
    pub segments: Vec<TripSegment>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TripSegment {
    pub position: i64,
    pub origin: String,
    pub destination: String,
    pub departure_date: String,
    pub candidates: Vec<TripCandidate>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TripCandidate {
    pub candidate: i64,
    pub chosen: bool,
    pub airline: String,
    /// Comma-separated and in order (`KL1007,KL0805`), because finalisation
    /// matches this against fresh search results.
    pub flight_numbers: String,
    /// The rendered line `Leg::itinerary` produces, for showing.
    pub itinerary: String,
    pub departing_at_local: Option<String>,
    pub arriving_at_local: Option<String>,
    pub duration_minutes: Option<i64>,
    /// What it cost when parked. Never refreshed — see the design doc.
    pub quoted_price: Option<f64>,
    pub quoted_currency: Option<String>,
    pub source: Option<String>,
}

/// A candidate on its way into the database.
#[derive(Debug, Clone, PartialEq)]
pub struct NewCandidate {
    pub airline: String,
    pub flight_numbers: String,
    pub itinerary: String,
    pub departing_at_local: Option<String>,
    pub arriving_at_local: Option<String>,
    pub duration_minutes: Option<i64>,
    pub quoted_price: Option<f64>,
    pub quoted_currency: Option<String>,
    pub source: Option<String>,
}

/// What a caller has already checked a flight against, for `add_candidate`
/// to verify again inside the same lock as the write it guards — see that
/// method's own comment for why the check cannot live only in the caller.
pub struct ExpectedSegment<'a> {
    pub origin: &'a str,
    pub destination: &'a str,
    /// `None` when the flight being added had no usable date to check —
    /// "nothing to verify", not "verified".
    pub departure_date: Option<&'a str>,
}

/// What happened when somebody pressed START on an invite link.
///
/// The three refusals that share one reply — unknown code, closed round,
/// full round — are one variant on purpose. Collapsing them here rather than
/// at the call site means no caller can accidentally tell a stranger which
/// of the three it was, and so whether a code they guessed exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    Admitted,
    AlreadyIn,
    Revoked,
    NoRoom,
}

/// One round as `/invite status` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundStatus {
    pub code: String,
    pub capacity: i64,
    /// Seats taken, counting revoked members: a round of 100 admits 100
    /// people once.
    pub used: i64,
    pub open: bool,
}

/// A numbered change to an existing database. `MIGRATIONS` above creates
/// tables and is safe to re-run; these are not, so each one runs at most
/// once and the number it reached is recorded.
///
/// Never renumber or edit a step that has shipped, and never insert one
/// below a number that has already run — the runner skips anything at or
/// below the recorded version, so it would be silently ignored. Append.
enum Step {
    Sql(&'static str),
    /// For work that needs a loop or a returned id — plain SQL cannot ask
    /// DuckDB for `nextval` per row and keep the mapping.
    #[allow(dead_code)]
    Code(fn(&Connection) -> Result<()>),
}

const STEP_1_NEW_TABLES: &str = r#"
CREATE SEQUENCE IF NOT EXISTS accounts_id_seq;
-- A person, independent of how they reach Scout. Deliberately almost empty:
-- everything knowable about someone belongs to one of their identities or to
-- their data, not here.
CREATE TABLE IF NOT EXISTS accounts (
    id         BIGINT PRIMARY KEY DEFAULT nextval('accounts_id_seq'),
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
-- One row per way of proving you are that account. `kind` is 'telegram'
-- today; a web login is a second kind. The primary key is what stops one
-- Telegram id from being claimed by two accounts.
CREATE TABLE IF NOT EXISTS identities (
    account_id  BIGINT NOT NULL,
    kind        TEXT NOT NULL,
    external_id TEXT NOT NULL,
    created_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (kind, external_id)
);
-- Where to reach an account on a channel, when nothing more specific says
-- otherwise. Replaces `user_chats`.
CREATE TABLE IF NOT EXISTS deliveries (
    account_id BIGINT NOT NULL,
    channel    TEXT NOT NULL,
    address    TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (account_id, channel)
);
CREATE SEQUENCE IF NOT EXISTS conversations_id_seq;
-- A rolling thread. `scope` keeps a group chat's history out of the
-- account's private thread: 'direct' is the 1:1 chat and the web app, which
-- share; a group is 'telegram:<chat_id>'.
CREATE TABLE IF NOT EXISTS conversations (
    id            BIGINT PRIMARY KEY DEFAULT nextval('conversations_id_seq'),
    account_id    BIGINT NOT NULL,
    scope         TEXT NOT NULL,
    pending_draft TEXT,
    started_at    TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at    TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE SEQUENCE IF NOT EXISTS messages_id_seq;
-- `body` is a serde_json `rig::completion::Message`. Storing the whole
-- message rather than plain text keeps tool calls and their results paired,
-- which `trim_history` depends on.
CREATE TABLE IF NOT EXISTS messages (
    id              BIGINT PRIMARY KEY DEFAULT nextval('messages_id_seq'),
    conversation_id BIGINT NOT NULL,
    position        BIGINT NOT NULL,
    body            TEXT NOT NULL,
    created_at      TIMESTAMP NOT NULL DEFAULT current_timestamp
);
"#;

fn steps() -> Vec<(i64, Step)> {
    vec![(1, Step::Sql(STEP_1_NEW_TABLES))]
}

fn apply_steps(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version BIGINT NOT NULL)")?;
    let mut stmt = conn.prepare("SELECT version FROM schema_version")?;
    let current: Option<i64> = stmt.query_map([], |r| r.get(0))?.next().transpose()?;
    drop(stmt);
    let mut current = match current {
        Some(v) => v,
        None => {
            conn.execute("INSERT INTO schema_version (version) VALUES (0)", [])?;
            0
        }
    };
    for (n, step) in steps() {
        if n <= current {
            continue;
        }
        // DDL is transactional in DuckDB v1.5.1 (measured), so a step that
        // fails half-way leaves the database exactly as it was.
        conn.execute_batch("BEGIN")?;
        let result = match step {
            Step::Sql(sql) => conn.execute_batch(sql).map_err(anyhow::Error::from),
            Step::Code(f) => f(conn),
        };
        if let Err(e) = result {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e.context(format!("migration step {n} failed")));
        }
        conn.execute("UPDATE schema_version SET version = ?", params![n])?;
        conn.execute_batch("COMMIT")?;
        current = n;
        tracing::info!(step = n, "applied migration step");
    }
    Ok(())
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref())?;
        conn.execute_batch(MIGRATIONS)?;
        apply_steps(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Highest migration step applied to this database.
    pub fn schema_version(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT version FROM schema_version", [], |r| r.get(0))?)
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

    /// Record one handled request for usage statistics.
    /// `request_log.kind` for one billable Duffel search. Kept here beside
    /// the table it is written into, because `/stat` reads it back by name
    /// and a typo on either side would silently report zero.
    pub const FLIGHT_SEARCH: &'static str = "flight_search";

    pub fn log_request(&self, user_id: i64, kind: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO request_log (user_id, kind) VALUES (?, ?)",
            params![user_id, kind],
        )?;
        Ok(())
    }

    /// Remember what to call a user id in `/stat`. Deliberately separate
    /// from `log_request`: commands should teach the bot your name without
    /// also counting as requests. A blank name is not a name.
    pub fn remember_user(&self, user_id: i64, display_name: &str) -> Result<()> {
        let name = display_name.trim();
        if name.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO users (user_id, display_name) VALUES (?, ?)
             ON CONFLICT (user_id)
             DO UPDATE SET display_name = excluded.display_name, updated_at = now()",
            params![user_id, name],
        )?;
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

    /// Records where this person last spoke, for announcements.
    ///
    /// A user id and a chat id are the same number in a private chat and
    /// different in a group, so the chat is stored rather than derived.
    pub fn remember_chat(&self, user_id: i64, chat_id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO user_chats (user_id, chat_id) VALUES (?, ?)
             ON CONFLICT (user_id)
             DO UPDATE SET chat_id = excluded.chat_id, updated_at = now()",
            params![user_id, chat_id],
        )?;
        Ok(())
    }

    /// Everyone the bot could announce something to, as (user, chat).
    pub fn broadcast_targets(&self) -> Result<Vec<(i64, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT user_id, chat_id FROM user_chats ORDER BY user_id ASC")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Everyone admitted and not since revoked. Read once at startup to
    /// build the membership set the gate actually consults; the table is
    /// the durable record, the set is the hot path.
    pub fn active_members(&self) -> Result<Vec<i64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT user_id FROM members WHERE revoked_at IS NULL ORDER BY user_id ASC",
        )?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Try to take a seat in `code` for `user_id`.
    ///
    /// One method, one lock acquisition, so check-and-insert is atomic by
    /// construction: a round of 100 admits exactly 100 however many people
    /// press START at the same moment. There is no counter to drift from
    /// the rows — seats used *is* `count(*)` over them.
    pub fn claim_seat(&self, user_id: i64, chat_id: i64, code: &str) -> Result<Claim> {
        let conn = self.conn.lock().unwrap();

        // Membership decides before the round does. Checking the round
        // first would let a revoked person be told "that round is full",
        // and a member re-clicking a link consume a second seat.
        let mut stmt = conn.prepare("SELECT revoked_at IS NULL FROM members WHERE user_id = ?")?;
        let standing: Option<bool> = stmt
            .query_map(params![user_id], |row| row.get(0))?
            .next()
            .transpose()?;
        match standing {
            Some(true) => return Ok(Claim::AlreadyIn),
            // Without this, revoking is theatre: the next link would let
            // them straight back in.
            Some(false) => return Ok(Claim::Revoked),
            None => {}
        }

        let mut stmt = conn.prepare(
            "SELECT r.open, r.capacity, (SELECT count(*) FROM members m WHERE m.code = r.code)
             FROM invite_rounds r WHERE r.code = ?",
        )?;
        let round: Option<(bool, i64, i64)> = stmt
            .query_map(params![code], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .next()
            .transpose()?;

        // Unknown code, closed round and full round are one outcome on
        // purpose: telling a stranger which it was says whether a code they
        // guessed exists, and that is information with no use.
        if !matches!(round, Some((true, capacity, used)) if used < capacity) {
            conn.execute(
                "INSERT INTO waitlist (user_id, chat_id, code) VALUES (?, ?, ?)
                 ON CONFLICT (user_id) DO UPDATE SET
                     chat_id = excluded.chat_id,
                     code = excluded.code,
                     -- They tried again and were turned away again, so they
                     -- are waiting again — but `seen_at` is untouched, so a
                     -- second attempt does not cost them their place.
                     invited_at = NULL",
                params![user_id, chat_id, code],
            )?;
            return Ok(Claim::NoRoom);
        }

        conn.execute(
            "INSERT INTO members (user_id, code) VALUES (?, ?)",
            params![user_id, code],
        )?;
        // Nobody who is inside should be chased by a later announce.
        conn.execute("DELETE FROM waitlist WHERE user_id = ?", params![user_id])?;
        Ok(Claim::Admitted)
    }

    /// Opens a round. False when the name is already taken — reusing one
    /// would silently pool two rounds' seats under a single capacity.
    pub fn create_round(&self, code: &str, capacity: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "INSERT INTO invite_rounds (code, capacity) VALUES (?, ?)
             ON CONFLICT (code) DO NOTHING",
            params![code, capacity],
        )?;
        Ok(changed == 1)
    }

    /// Stops or resumes admitting. False when there is no such round.
    pub fn set_round_open(&self, code: &str, open: bool) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE invite_rounds SET open = ? WHERE code = ?",
            params![open, code],
        )?;
        Ok(changed > 0)
    }

    /// Every round, oldest first, with seats counted from the member rows.
    pub fn rounds(&self) -> Result<Vec<RoundStatus>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT r.code, r.capacity, r.open,
                    (SELECT count(*) FROM members m WHERE m.code = r.code)
             FROM invite_rounds r ORDER BY r.created_at ASC, r.code ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(RoundStatus {
                code: row.get(0)?,
                capacity: row.get(1)?,
                open: row.get(2)?,
                used: row.get(3)?,
            })
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// How many people are queued and have not been told about a new round.
    pub fn waiting_count(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT count(*) FROM waitlist WHERE invited_at IS NULL")?;
        let n: i64 = stmt.query_map([], |row| row.get(0))?.next().transpose()?.unwrap_or(0);
        Ok(n)
    }

    /// Removes a member. False when they were not one (or already were
    /// removed). The row stays: the seat is spent, and moderation must not
    /// quietly reopen a round.
    pub fn revoke(&self, user_id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE members SET revoked_at = current_timestamp
             WHERE user_id = ? AND revoked_at IS NULL",
            params![user_id],
        )?;
        Ok(changed > 0)
    }

    /// Undoes a revoke. False when they were not revoked. Consumes no seat,
    /// for the same reason revoking returned none.
    pub fn restore(&self, user_id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE members SET revoked_at = NULL WHERE user_id = ? AND revoked_at IS NOT NULL",
            params![user_id],
        )?;
        Ok(changed > 0)
    }

    /// Who an announce should reach, as (user, chat), oldest first — so if
    /// the new round is smaller than the queue, the people who have waited
    /// longest hear first.
    pub fn waitlist_to_invite(&self) -> Result<Vec<(i64, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT user_id, chat_id FROM waitlist WHERE invited_at IS NULL
             ORDER BY seen_at ASC, user_id ASC",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Stamped per successful send, so re-running an announce reaches only
    /// the people the first run missed.
    pub fn mark_invited(&self, user_id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE waitlist SET invited_at = current_timestamp WHERE user_id = ?",
            params![user_id],
        )?;
        Ok(())
    }

    /// Drops somebody from the queue. Used when a send proves they cannot
    /// be reached at all — they blocked the bot or deleted the chat, which
    /// is an opt-out, and carrying them forward would mean retrying that
    /// same failure at every future round.
    pub fn forget_waitlist(&self, user_id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM waitlist WHERE user_id = ?", params![user_id])?;
        Ok(())
    }

    /// Requests this user has made since midnight, for the daily cap.
    ///
    /// Reactions and flight searches are excluded: a reaction is not a
    /// request, and a flight search is a sub-event of a message already
    /// counted, so counting it would charge one message twice.
    ///
    /// `current_date` is the container clock, which runs UTC — the same
    /// midnight the reply tells the user about.
    pub fn requests_today(&self, user_id: i64) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT count(*) FROM request_log
             WHERE user_id = ? AND kind IN ('text', 'photo') AND created_at >= current_date",
        )?;
        let n: i64 = stmt
            .query_map(params![user_id], |row| row.get(0))?
            .next()
            .transpose()?
            .unwrap_or(0);
        Ok(n)
    }

    /// Flight searches per user since `cutoff`, scoped to one user. See
    /// [`FLIGHT_SEARCH`].
    pub fn flight_searches_for(&self, cutoff: &str, user_id: i64) -> Result<BTreeMap<i64, i64>> {
        self.kind_counts(cutoff, Self::FLIGHT_SEARCH, Some(user_id))
    }

    /// The same across every user. Reachable from `/stat` only when the
    /// caller is an admin — like `usage_stats_all`, this method and its
    /// callers are the access-control surface for cross-user data.
    pub fn flight_searches_all(&self, cutoff: &str) -> Result<BTreeMap<i64, i64>> {
        self.kind_counts(cutoff, Self::FLIGHT_SEARCH, None)
    }

    /// Requests of one `kind` per user, optionally narrowed to a single
    /// user. `None` means every user, so callers pass `Some` unless they
    /// have already checked the caller is allowed to see everyone.
    fn kind_counts(
        &self,
        cutoff: &str,
        kind: &str,
        user_id: Option<i64>,
    ) -> Result<BTreeMap<i64, i64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT user_id, count(*) FROM request_log
             WHERE kind = ? AND created_at >= CAST(? AS TIMESTAMP)
               AND (? IS NULL OR user_id = ?)
             GROUP BY user_id ORDER BY user_id ASC",
        )?;
        let rows = stmt.query_map(params![kind, cutoff, user_id, user_id], |row| {
            Ok((row.get(0)?, row.get(1)?))
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

    /// Creates the trip if the name is new, otherwise updates only what was
    /// supplied. Two statements rather than one `ON CONFLICT DO UPDATE`: with
    /// upsert, an unsupplied `adults` would arrive as the insert's default and
    /// overwrite a value already set.
    pub fn upsert_trip(
        &self,
        user_id: i64,
        name: &str,
        adults: Option<i64>,
        cabin_class: Option<&str>,
    ) -> Result<Trip> {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("a trip needs a name — ask the traveller what to call it");
        }
        let key = name.to_lowercase();
        let conn = self.conn.lock().unwrap();
        let inserted = conn.execute(
            "INSERT INTO trips (user_id, name, name_key) VALUES (?, ?, ?)
             ON CONFLICT (user_id, name_key) DO NOTHING",
            params![user_id, name, key],
        )?;
        // A freshly created trip already starts in `planning`, and a call
        // that supplies neither field is how find-or-create works — it must
        // stay inert. Only an edit to an *existing* trip's price-relevant
        // fields can invalidate prices it was finalised at.
        let invalidates_prices = inserted == 0 && (adults.is_some() || cabin_class.is_some());
        if let Some(adults) = adults {
            conn.execute(
                "UPDATE trips SET adults = ?, updated_at = current_timestamp
                 WHERE user_id = ? AND name_key = ?",
                params![adults, user_id, key],
            )?;
        }
        if let Some(cabin) = cabin_class {
            conn.execute(
                "UPDATE trips SET cabin_class = ?, updated_at = current_timestamp
                 WHERE user_id = ? AND name_key = ?",
                params![cabin, user_id, key],
            )?;
        }
        if invalidates_prices {
            conn.execute(
                "UPDATE trips SET status = 'planning', updated_at = current_timestamp
                 WHERE user_id = ? AND name_key = ?",
                params![user_id, key],
            )?;
        }
        let id: i64 = conn.query_row(
            "SELECT id FROM trips WHERE user_id = ? AND name_key = ?",
            params![user_id, key],
            |row| row.get(0),
        )?;
        load_trip(&conn, id)
    }

    /// Used by finalisation to record that a trip has been priced.
    pub fn set_trip_status(&self, trip_id: i64, status: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE trips SET status = ?, updated_at = current_timestamp WHERE id = ?",
            params![status, trip_id],
        )?;
        Ok(())
    }

    pub fn find_trip(&self, user_id: i64, name: &str) -> Result<Option<Trip>> {
        let key = name.trim().to_lowercase();
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id FROM trips WHERE user_id = ? AND name_key = ?")?;
        let id: Option<i64> = stmt
            .query_map(params![user_id, key], |row| row.get(0))?
            .next()
            .transpose()?;
        match id {
            Some(id) => Ok(Some(load_trip(&conn, id)?)),
            None => Ok(None),
        }
    }

    /// Every trip this user has, newest activity first.
    pub fn list_trips(&self, user_id: i64) -> Result<Vec<Trip>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id FROM trips WHERE user_id = ? ORDER BY updated_at DESC, id DESC")?;
        let ids: Vec<i64> = stmt
            .query_map(params![user_id], |row| row.get(0))?
            .collect::<duckdb::Result<_>>()?;
        ids.into_iter().map(|id| load_trip(&conn, id)).collect()
    }

    /// Appends when `position` is None, otherwise inserts there and shifts the
    /// rest down. Candidates move with their segment: they are keyed by
    /// position, so a shift that forgot them would reattach somebody's chosen
    /// flight to a different route.
    pub fn add_segment(
        &self,
        trip_id: i64,
        position: Option<i64>,
        origin: &str,
        destination: &str,
        departure_date: &str,
    ) -> Result<Trip> {
        let conn = self.conn.lock().unwrap();
        // Checked before anything is written: without this, a bad trip_id
        // still passed the count query (as 0) and reached the INSERT below,
        // leaving a segment row for a trip that does not exist — and no read
        // path can ever find it again, because every read goes through a trip.
        let known: i64 = conn.query_row(
            "SELECT count(*) FROM trips WHERE id = ?",
            params![trip_id],
            |row| row.get(0),
        )?;
        if known == 0 {
            anyhow::bail!("no such trip");
        }
        let count: i64 = conn.query_row(
            "SELECT count(*) FROM trip_segments WHERE trip_id = ?",
            params![trip_id],
            |row| row.get(0),
        )?;
        let at = match position {
            Some(p) if p >= 1 && p <= count => p,
            Some(p) if p == count + 1 => p,
            Some(p) => {
                let noun = if count == 1 { "segment" } else { "segments" };
                anyhow::bail!(
                    "this trip has {count} {noun}, so position {p} is not somewhere to put one"
                )
            }
            None => count + 1,
        };
        if at <= count {
            // Descending is not needed: DuckDB applies this set-wise, so no
            // intermediate state can collide with the primary key.
            conn.execute(
                "UPDATE trip_segments SET position = position + 1
                 WHERE trip_id = ? AND position >= ?",
                params![trip_id, at],
            )?;
            conn.execute(
                "UPDATE segment_candidates SET position = position + 1
                 WHERE trip_id = ? AND position >= ?",
                params![trip_id, at],
            )?;
        }
        conn.execute(
            "INSERT INTO trip_segments (trip_id, position, origin, destination, departure_date)
             VALUES (?, ?, ?, ?, ?)",
            params![trip_id, at, origin, destination, departure_date],
        )?;
        touch(&conn, trip_id)?;
        load_trip(&conn, trip_id)
    }

    /// Changes where or when one segment goes, leaving the rest alone.
    ///
    /// Returns the trip and how many parked options were dropped by the
    /// change. They have to go: an option is a flight on a particular route
    /// and day, and `add_candidate` would refuse to attach it to these
    /// values now for exactly that reason. Reporting the count is what stops
    /// them vanishing silently.
    ///
    /// A change that changes nothing keeps them — restating a date must not
    /// cost the traveller their shortlist.
    pub fn update_segment(
        &self,
        trip_id: i64,
        position: i64,
        origin: Option<&str>,
        destination: Option<&str>,
        departure_date: Option<&str>,
    ) -> Result<(Trip, usize, bool)> {
        let conn = self.conn.lock().unwrap();
        let current = conn
            .query_row(
                "SELECT origin, destination, departure_date FROM trip_segments
                 WHERE trip_id = ? AND position = ?",
                params![trip_id, position],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .map_err(|_| anyhow::anyhow!("this trip has no segment {position}"))?;

        let wanted = (
            origin.unwrap_or(&current.0).to_string(),
            destination.unwrap_or(&current.1).to_string(),
            departure_date.unwrap_or(&current.2).to_string(),
        );
        if wanted == current {
            // Asking for what is already true is not a failure, and the
            // caller has to be able to tell the two apart.
            return Ok((load_trip(&conn, trip_id)?, 0, false));
        }

        conn.execute(
            "UPDATE trip_segments SET origin = ?, destination = ?, departure_date = ?
             WHERE trip_id = ? AND position = ?",
            params![wanted.0, wanted.1, wanted.2, trip_id, position],
        )?;
        let dropped = conn.execute(
            "DELETE FROM segment_candidates WHERE trip_id = ? AND position = ?",
            params![trip_id, position],
        )?;
        touch(&conn, trip_id)?;
        Ok((load_trip(&conn, trip_id)?, dropped, true))
    }

    pub fn drop_segment(&self, trip_id: i64, position: i64) -> Result<Trip> {
        let conn = self.conn.lock().unwrap();
        let removed = conn.execute(
            "DELETE FROM trip_segments WHERE trip_id = ? AND position = ?",
            params![trip_id, position],
        )?;
        if removed == 0 {
            anyhow::bail!("this trip has no segment {position}");
        }
        conn.execute(
            "DELETE FROM segment_candidates WHERE trip_id = ? AND position = ?",
            params![trip_id, position],
        )?;
        // Closing the gap keeps positions contiguous, which is the invariant
        // that makes the shift above correct.
        conn.execute(
            "UPDATE trip_segments SET position = position - 1 WHERE trip_id = ? AND position > ?",
            params![trip_id, position],
        )?;
        conn.execute(
            "UPDATE segment_candidates SET position = position - 1
             WHERE trip_id = ? AND position > ?",
            params![trip_id, position],
        )?;
        touch(&conn, trip_id)?;
        load_trip(&conn, trip_id)
    }

    /// Parks a flight against a segment. `decided` also marks it chosen, so
    /// the common single-option path is one call.
    ///
    /// `expected` is checked against the segment's row in this same lock
    /// acquisition, not against a `Trip` the caller read earlier: that read
    /// and this write are two separate lock acquisitions, so a concurrent
    /// `add_trip_segment` or `drop_trip_segment` renumbering positions in
    /// between could otherwise land a candidate validated against one route
    /// onto a segment that is now something else. The caller passes what it
    /// validated; this re-checks that it is still true.
    ///
    /// Candidate numbers are never reused: they are what the traveller sees and
    /// what `choose_candidate` takes, and recycling one would silently retarget
    /// a decision made against the old numbering.
    pub fn add_candidate(
        &self,
        trip_id: i64,
        position: i64,
        expected: ExpectedSegment,
        new: NewCandidate,
        decided: bool,
    ) -> Result<Trip> {
        let conn = self.conn.lock().unwrap();
        // Distinguished from the segment check below: "no such trip" and
        // "this trip has no segment N" point the caller at different fixes.
        let known: i64 = conn.query_row(
            "SELECT count(*) FROM trips WHERE id = ?",
            params![trip_id],
            |row| row.get(0),
        )?;
        if known == 0 {
            anyhow::bail!("no such trip");
        }
        let mut stmt = conn.prepare(
            "SELECT origin, destination, departure_date FROM trip_segments
             WHERE trip_id = ? AND position = ?",
        )?;
        let segment: Option<(String, String, String)> = stmt
            .query_map(params![trip_id, position], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .next()
            .transpose()?;
        drop(stmt);
        let Some((origin, destination, departure_date)) = segment else {
            anyhow::bail!("this trip has no segment {position}");
        };
        // The route and date guard: nothing else stops a flight validated
        // against one segment being written to a different one by the time
        // this lock is taken.
        if origin != expected.origin || destination != expected.destination {
            anyhow::bail!(
                "segment {position} is {origin}→{destination} but that flight is \
                 {}→{}",
                expected.origin,
                expected.destination
            );
        }
        if let Some(expected_date) = expected.departure_date {
            if departure_date != expected_date {
                anyhow::bail!(
                    "segment {position} departs {departure_date} but that flight departs \
                     {expected_date}"
                );
            }
        }
        // next_candidate is a high-water mark on the segment row: read and
        // advance it here, inside the lock this method already holds, so a
        // dropped candidate's number is never handed to the next insert.
        let next: i64 = conn.query_row(
            "SELECT next_candidate FROM trip_segments WHERE trip_id = ? AND position = ?",
            params![trip_id, position],
            |row| row.get(0),
        )?;
        conn.execute(
            "UPDATE trip_segments SET next_candidate = next_candidate + 1
             WHERE trip_id = ? AND position = ?",
            params![trip_id, position],
        )?;
        conn.execute(
            "INSERT INTO segment_candidates (
                 trip_id, position, candidate, chosen, airline, flight_numbers, itinerary,
                 departing_at_local, arriving_at_local, duration_minutes,
                 quoted_price, quoted_currency, quoted_at, source)
             VALUES (?, ?, ?, false, ?, ?, ?, ?, ?, ?, ?, ?, current_timestamp, ?)",
            params![
                trip_id,
                position,
                next,
                new.airline,
                new.flight_numbers,
                new.itinerary,
                new.departing_at_local,
                new.arriving_at_local,
                new.duration_minutes,
                new.quoted_price,
                new.quoted_currency,
                new.source
            ],
        )?;
        if decided {
            choose_within(&conn, trip_id, position, next)?;
        }
        touch(&conn, trip_id)?;
        load_trip(&conn, trip_id)
    }

    pub fn choose_candidate(&self, trip_id: i64, position: i64, candidate: i64) -> Result<Trip> {
        let conn = self.conn.lock().unwrap();
        // choose_within only checks the segment_candidates row, which a
        // deleted trip has none of — indistinguishable, from there, from a
        // numbering mistake on a trip that still exists. Checked here,
        // separately, so a trip gone by the time this runs reads as "no
        // such trip" rather than a bad option number.
        let known: i64 = conn.query_row(
            "SELECT count(*) FROM trips WHERE id = ?",
            params![trip_id],
            |row| row.get(0),
        )?;
        if known == 0 {
            anyhow::bail!("no such trip");
        }
        choose_within(&conn, trip_id, position, candidate)?;
        touch(&conn, trip_id)?;
        load_trip(&conn, trip_id)
    }

    pub fn drop_candidate(&self, trip_id: i64, position: i64, candidate: i64) -> Result<Trip> {
        let conn = self.conn.lock().unwrap();
        let removed = conn.execute(
            "DELETE FROM segment_candidates WHERE trip_id = ? AND position = ? AND candidate = ?",
            params![trip_id, position, candidate],
        )?;
        if removed == 0 {
            anyhow::bail!("segment {position} has no option {candidate}");
        }
        touch(&conn, trip_id)?;
        load_trip(&conn, trip_id)
    }

    /// False when there was no such trip. Deleting something already gone is
    /// the state the caller wanted, not a failure.
    pub fn delete_trip(&self, user_id: i64, name: &str) -> Result<bool> {
        let key = name.trim().to_lowercase();
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id FROM trips WHERE user_id = ? AND name_key = ?")?;
        let mut ids = stmt.query_map(params![user_id, key], |row| row.get::<_, i64>(0))?;
        let Some(id) = ids.next().transpose()? else {
            return Ok(false);
        };
        drop(ids);
        drop(stmt);
        conn.execute("DELETE FROM segment_candidates WHERE trip_id = ?", params![id])?;
        conn.execute("DELETE FROM trip_segments WHERE trip_id = ?", params![id])?;
        conn.execute("DELETE FROM trips WHERE id = ?", params![id])?;
        Ok(true)
    }
}

/// Clears the position's flags and sets one. "At most one chosen" cannot be
/// a `UNIQUE` constraint because `false` repeats, so it is this function's
/// job — and the caller always holds the connection lock, which is what
/// makes the pair of statements indivisible.
fn choose_within(conn: &Connection, trip_id: i64, position: i64, candidate: i64) -> Result<()> {
    let known: i64 = conn.query_row(
        "SELECT count(*) FROM segment_candidates
         WHERE trip_id = ? AND position = ? AND candidate = ?",
        params![trip_id, position, candidate],
        |row| row.get(0),
    )?;
    if known == 0 {
        anyhow::bail!("segment {position} has no option {candidate}");
    }
    conn.execute(
        "UPDATE segment_candidates SET chosen = false WHERE trip_id = ? AND position = ?",
        params![trip_id, position],
    )?;
    conn.execute(
        "UPDATE segment_candidates SET chosen = true
         WHERE trip_id = ? AND position = ? AND candidate = ?",
        params![trip_id, position, candidate],
    )?;
    Ok(())
}

/// Marks a trip edited. Status goes back to `planning` because whatever it
/// was priced at no longer describes it.
fn touch(conn: &Connection, trip_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE trips SET status = 'planning', updated_at = current_timestamp WHERE id = ?",
        params![trip_id],
    )?;
    Ok(())
}

/// Reads one whole trip. Takes `&Connection` rather than `&Store` so it can
/// be called by a method that already holds the lock — every trip-mutating
/// method returns the trip it just changed, and re-locking would deadlock.
fn load_trip(conn: &Connection, id: i64) -> Result<Trip> {
    let (name, adults, cabin_class, status) = conn.query_row(
        "SELECT name, adults, cabin_class, status FROM trips WHERE id = ?",
        params![id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        },
    )?;

    let mut stmt = conn.prepare(
        "SELECT position, origin, destination, departure_date FROM trip_segments
         WHERE trip_id = ? ORDER BY position",
    )?;
    let rows: Vec<(i64, String, String, String)> = stmt
        .query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<duckdb::Result<_>>()?;

    let mut stmt = conn.prepare(
        "SELECT position, candidate, chosen, airline, flight_numbers, itinerary,
                departing_at_local, arriving_at_local, duration_minutes,
                quoted_price, quoted_currency, source
         FROM segment_candidates WHERE trip_id = ? ORDER BY position, candidate",
    )?;
    let candidates: Vec<(i64, TripCandidate)> = stmt
        .query_map(params![id], |r| {
            Ok((
                r.get(0)?,
                TripCandidate {
                    candidate: r.get(1)?,
                    chosen: r.get(2)?,
                    airline: r.get(3)?,
                    flight_numbers: r.get(4)?,
                    itinerary: r.get(5)?,
                    departing_at_local: r.get(6)?,
                    arriving_at_local: r.get(7)?,
                    duration_minutes: r.get(8)?,
                    quoted_price: r.get(9)?,
                    quoted_currency: r.get(10)?,
                    source: r.get(11)?,
                },
            ))
        })?
        .collect::<duckdb::Result<_>>()?;

    let segments = rows
        .into_iter()
        .map(|(position, origin, destination, departure_date)| TripSegment {
            position,
            origin,
            destination,
            departure_date,
            candidates: candidates
                .iter()
                .filter(|(p, _)| *p == position)
                .map(|(_, c)| c.clone())
                .collect(),
        })
        .collect();

    Ok(Trip { id, name, adults, cabin_class, status, segments })
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
    fn the_new_tables_exist_after_opening() {
        let (s, _d) = test_store();
        let conn = s.conn.lock().unwrap();
        for table in ["accounts", "identities", "deliveries", "conversations", "messages"] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM information_schema.tables WHERE table_name = ?",
                    params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "{table} should exist");
        }
    }

    #[test]
    fn migration_steps_run_once_and_only_once() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.duckdb");

        let s = Store::open(&path).unwrap();
        let first = s.schema_version().unwrap();
        drop(s);

        // Re-opening must not re-run anything. A step that ran twice would
        // duplicate backfilled rows, so the version is the guard.
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), first);
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
    fn broadcast_targets_are_the_chats_people_actually_talk_in() {
        // A user id is not a chat id — they coincide in a private chat and
        // do not in a group. Announcements have to go where the
        // conversation happened, so the chat is recorded rather than
        // assumed.
        let (s, _d) = test_store();
        s.remember_chat(1, 1).unwrap();
        s.remember_chat(2, -100200300).unwrap();
        assert_eq!(s.broadcast_targets().unwrap(), vec![(1, 1), (2, -100200300)]);

        // Moving chats replaces the old one: an announcement should follow
        // the person, not accumulate copies.
        s.remember_chat(1, -999).unwrap();
        assert_eq!(s.broadcast_targets().unwrap(), vec![(1, -999), (2, -100200300)]);
    }

    #[test]
    fn flight_searches_are_counted_apart_from_ordinary_requests() {
        // Every flight search is billed by Duffel, so these need their own
        // number rather than being buried in the request total.
        let (s, _d) = test_store();
        s.log_request_at(1, "text", "2026-07-25 10:00:00").unwrap();
        s.log_request_at(1, Store::FLIGHT_SEARCH, "2026-07-25 10:01:00").unwrap();
        s.log_request_at(1, Store::FLIGHT_SEARCH, "2026-07-25 10:02:00").unwrap();
        s.log_request_at(2, Store::FLIGHT_SEARCH, "2026-07-25 12:00:00").unwrap();
        s.log_request_at(1, Store::FLIGHT_SEARCH, "2026-07-20 09:00:00").unwrap(); // before cutoff

        let all = s.flight_searches_all("2026-07-25 00:00:00").unwrap();
        assert_eq!(all.get(&1), Some(&2), "the text request must not be counted");
        assert_eq!(all.get(&2), Some(&1));

        // Same access-control split as usage_stats: an ordinary caller sees
        // only their own.
        let mine = s.flight_searches_for("2026-07-25 00:00:00", 1).unwrap();
        assert_eq!(mine.get(&1), Some(&2));
        assert_eq!(mine.get(&2), None, "user 1 must not see user 2's searches");

        assert!(s
            .flight_searches_for("2026-07-25 00:00:00", 99)
            .unwrap()
            .is_empty());
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
        s.log_request(3, "reaction").unwrap();
        let rows = s.usage_stats_all("2000-01-01 00:00:00").unwrap();
        assert!(rows.iter().any(|(u, _, _)| *u == 3));
    }

    #[test]
    fn display_names_track_the_latest_seen_name() {
        let (s, _d) = test_store();
        s.remember_user(1, "@alice").unwrap();
        s.remember_user(2, "Bob Jansen").unwrap();
        // Blank is not a name — the row is left absent so /stat falls back
        // to the bare id rather than printing an empty column.
        s.remember_user(4, "   ").unwrap();

        let names = s.display_names().unwrap();
        assert_eq!(names.get(&1).map(String::as_str), Some("@alice"));
        assert_eq!(names.get(&2).map(String::as_str), Some("Bob Jansen"));
        assert_eq!(names.get(&4), None);

        // Renaming overwrites rather than accumulating rows.
        s.remember_user(1, "@alice_new").unwrap();
        let names = s.display_names().unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names.get(&1).map(String::as_str), Some("@alice_new"));
    }

    #[test]
    fn logging_a_request_does_not_invent_a_name() {
        // Requests and names are recorded independently: someone can appear
        // in /stat's counts long before the bot knows what to call them.
        let (s, _d) = test_store();
        s.log_request(7, "text").unwrap();
        assert!(s.display_names().unwrap().is_empty());
        assert_eq!(s.usage_stats_all("2000-01-01 00:00:00").unwrap().len(), 1);
    }

    #[test]
    fn forget_fact_removes_and_reports() {
        let (s, _d) = test_store();
        s.upsert_fact(1, "budget_style", "prefers cheap used gear").unwrap();
        assert!(s.forget_fact(1, "budget_style").unwrap());
        assert!(!s.forget_fact(1, "budget_style").unwrap());
        assert!(s.list_facts(1).unwrap().is_empty());
    }

    #[test]
    fn a_trip_is_found_by_name_case_insensitively_and_scoped_to_its_owner() {
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "September", None, None).unwrap();
        assert_eq!(trip.name, "September");
        assert_eq!(trip.adults, 1, "one adult unless said otherwise");
        assert_eq!(trip.status, "planning");
        assert!(trip.segments.is_empty());

        assert!(store.find_trip(7, "september").unwrap().is_some(), "names are not case-sensitive");
        assert!(store.find_trip(8, "September").unwrap().is_none(), "another user has no such trip");

        // Same name twice is the same trip, not a second one.
        store.upsert_trip(7, "SEPTEMBER", Some(2), Some("business")).unwrap();
        assert_eq!(store.list_trips(7).unwrap().len(), 1);
        let trip = store.find_trip(7, "September").unwrap().unwrap();
        assert_eq!(trip.adults, 2);
        assert_eq!(trip.cabin_class.as_deref(), Some("business"));
        assert_eq!(trip.name, "September", "the original spelling is kept");

        // Two users may each have a "September".
        store.upsert_trip(8, "September", None, None).unwrap();
        assert_eq!(store.list_trips(8).unwrap().len(), 1);
    }

    #[test]
    fn upserting_one_field_leaves_the_other_field_untouched() {
        // Regression guard for the two-statement design: a naive single
        // `ON CONFLICT DO UPDATE SET adults = ?, cabin_class = ?` would
        // silently null out whichever field this call didn't mention.
        let (store, _d) = test_store();
        store.upsert_trip(7, "September", Some(2), Some("business")).unwrap();

        let trip = store.upsert_trip(7, "September", Some(3), None).unwrap();
        assert_eq!(trip.adults, 3);
        assert_eq!(
            trip.cabin_class.as_deref(),
            Some("business"),
            "cabin class must survive an upsert that didn't mention it"
        );

        let trip = store.upsert_trip(7, "September", None, Some("economy")).unwrap();
        assert_eq!(trip.adults, 3, "adults must survive an upsert that didn't mention it");
        assert_eq!(trip.cabin_class.as_deref(), Some("economy"));
    }

    #[test]
    fn changing_adults_or_cabin_class_resets_a_finalised_trip_to_planning() {
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "September", Some(2), Some("business")).unwrap();

        store.set_trip_status(trip.id, "finalised").unwrap();
        let trip = store.upsert_trip(7, "September", Some(3), None).unwrap();
        assert_eq!(
            trip.status, "planning",
            "changing the passenger count invalidates a finalised trip's prices"
        );

        store.set_trip_status(trip.id, "finalised").unwrap();
        let trip = store.upsert_trip(7, "September", None, Some("economy")).unwrap();
        assert_eq!(
            trip.status, "planning",
            "changing cabin class invalidates a finalised trip's prices"
        );

        // find-or-create — supplying neither field — must stay inert.
        store.set_trip_status(trip.id, "finalised").unwrap();
        let trip = store.upsert_trip(7, "September", None, None).unwrap();
        assert_eq!(
            trip.status, "finalised",
            "an upsert with nothing to change must not reset status"
        );
    }

    #[test]
    fn segments_stay_contiguous_through_inserts_and_drops() {
        // Positions are how the traveller refers to a segment ("drop the
        // second leg"), so a hole would make every later instruction target
        // the wrong row.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "September", None, None).unwrap();
        for (o, d, date) in [("AMS", "LIS", "2026-09-03"), ("LIS", "FCO", "2026-09-07")] {
            store.add_segment(trip.id, None, o, d, date).unwrap();
        }
        let trip = store.add_segment(trip.id, Some(2), "BCN", "MAD", "2026-09-05").unwrap();
        assert_eq!(
            trip.segments.iter().map(|s| (s.position, s.origin.as_str())).collect::<Vec<_>>(),
            vec![(1, "AMS"), (2, "BCN"), (3, "LIS")],
            "inserting at 2 shifts the rest down rather than colliding"
        );

        let trip = store.drop_segment(trip.id, 1).unwrap();
        assert_eq!(
            trip.segments.iter().map(|s| (s.position, s.origin.as_str())).collect::<Vec<_>>(),
            vec![(1, "BCN"), (2, "LIS")],
            "dropping the first renumbers what is left from 1"
        );

        // A position nobody has is refused rather than silently doing nothing.
        assert!(store.drop_segment(trip.id, 9).is_err());
    }

    #[test]
    fn editing_a_trip_puts_it_back_to_planning() {
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "September", None, None).unwrap();
        store.add_segment(trip.id, None, "AMS", "LIS", "2026-09-03").unwrap();
        store.set_trip_status(trip.id, "finalised").unwrap();
        assert_eq!(store.find_trip(7, "September").unwrap().unwrap().status, "finalised");

        let trip = store.add_segment(trip.id, None, "LIS", "AMS", "2026-09-10").unwrap();
        assert_eq!(trip.status, "planning", "the trip changed, so its pricing no longer describes it");
    }

    #[test]
    fn adding_a_segment_to_a_nonexistent_trip_fails_without_writing_anything() {
        // The insert used to run before any existence check, so a bad
        // trip_id left a segment row that could never be read back — every
        // read path goes through a trip, and this trip does not exist.
        let (store, _d) = test_store();
        assert!(store.add_segment(999, None, "AMS", "LIS", "2026-09-03").is_err());

        let conn = store.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM trip_segments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "a failed add_segment must not leave an orphaned row");
    }

    fn candidate(airline: &str, numbers: &str, price: f64) -> NewCandidate {
        NewCandidate {
            airline: airline.to_string(),
            flight_numbers: numbers.to_string(),
            itinerary: format!("{numbers} somewhere"),
            departing_at_local: Some("2026-09-03T10:05:00".to_string()),
            arriving_at_local: Some("2026-09-03T12:15:00".to_string()),
            duration_minutes: Some(130),
            quoted_price: Some(price),
            quoted_currency: Some("EUR".to_string()),
            source: Some("duffel".to_string()),
        }
    }

    #[test]
    fn a_segment_holds_several_options_and_at_most_one_is_chosen() {
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "Japan", None, None).unwrap();
        let trip = store.add_segment(trip.id, None, "AMS", "NRT", "2026-09-03").unwrap();

        // Parked undecided: the traveller is comparing a nonstop against a
        // one-stop through Hong Kong.
        store
            .add_candidate(
                trip.id,
                1,
                ExpectedSegment { origin: "AMS", destination: "NRT", departure_date: Some("2026-09-03") },
                candidate("KLM", "KL861", 940.0),
                false,
            )
            .unwrap();
        let trip = store
            .add_candidate(
                trip.id,
                1,
                ExpectedSegment { origin: "AMS", destination: "NRT", departure_date: Some("2026-09-03") },
                candidate("Cathay", "CX270,CX500", 780.0),
                false,
            )
            .unwrap();
        let options = &trip.segments[0].candidates;
        assert_eq!(options.len(), 2);
        assert_eq!(options.iter().map(|c| c.candidate).collect::<Vec<_>>(), vec![1, 2]);
        assert!(options.iter().all(|c| !c.chosen), "nothing decided yet");

        let trip = store.choose_candidate(trip.id, 1, 2).unwrap();
        let options = &trip.segments[0].candidates;
        assert!(!options[0].chosen);
        assert!(options[1].chosen);

        // Choosing again moves the flag rather than setting a second one.
        let trip = store.choose_candidate(trip.id, 1, 1).unwrap();
        let options = &trip.segments[0].candidates;
        assert_eq!(options.iter().filter(|c| c.chosen).count(), 1);
        assert!(options[0].chosen);

        // A candidate the segment does not have.
        assert!(store.choose_candidate(trip.id, 1, 9).is_err());

        let trip = store.drop_candidate(trip.id, 1, 1).unwrap();
        assert_eq!(trip.segments[0].candidates.len(), 1, "the segment survives losing an option");
        assert_eq!(
            trip.segments[0].candidates[0].candidate, 2,
            "dropping the lower-numbered candidate must not renumber the one that is left"
        );
    }

    #[test]
    fn a_dropped_candidate_number_is_never_handed_to_a_later_one() {
        // Candidate numbers are traveller-facing: "go with option 2" refers
        // to a number, not a position in a list. Deriving the next number
        // from max(candidate) over the *live* rows recycles a number the
        // moment its holder is dropped — so a traveller who was shown
        // "option 2", dropped it, and later says "go with option 2" would
        // silently be given a different flight under the same name.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "Japan", None, None).unwrap();
        let trip = store.add_segment(trip.id, None, "AMS", "NRT", "2026-09-03").unwrap();
        store
            .add_candidate(
                trip.id,
                1,
                ExpectedSegment { origin: "AMS", destination: "NRT", departure_date: Some("2026-09-03") },
                candidate("KLM", "KL861", 940.0),
                false,
            )
            .unwrap();
        store
            .add_candidate(
                trip.id,
                1,
                ExpectedSegment { origin: "AMS", destination: "NRT", departure_date: Some("2026-09-03") },
                candidate("Cathay", "CX270,CX500", 780.0),
                false,
            )
            .unwrap();

        // Drop the highest-numbered candidate, then add a fresh one.
        store.drop_candidate(trip.id, 1, 2).unwrap();
        let trip = store
            .add_candidate(
                trip.id,
                1,
                ExpectedSegment { origin: "AMS", destination: "NRT", departure_date: Some("2026-09-03") },
                candidate("ANA", "NH205", 900.0),
                false,
            )
            .unwrap();

        let numbers: Vec<i64> = trip.segments[0].candidates.iter().map(|c| c.candidate).collect();
        assert_eq!(
            numbers,
            vec![1, 3],
            "the new candidate must take the next never-used number, not the one just dropped (2)"
        );
    }

    #[test]
    fn adding_a_candidate_reports_a_missing_trip_separately_from_a_missing_segment() {
        // Both counts can read 0, but they call for different fixes — one
        // means the trip id is wrong, the other that the segment position
        // is. Collapsing them into one message ("no segment N") would send
        // a caller with a bad trip id looking for a segment that was never
        // going to exist.
        let (store, _d) = test_store();
        let err = store
            .add_candidate(
                999,
                1,
                ExpectedSegment { origin: "AMS", destination: "NRT", departure_date: Some("2026-09-03") },
                candidate("KLM", "KL861", 940.0),
                false,
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "no such trip", "a nonexistent trip must not be reported as a missing segment");

        let trip = store.upsert_trip(7, "Japan", None, None).unwrap();
        let err = store
            .add_candidate(
                trip.id,
                1,
                ExpectedSegment { origin: "AMS", destination: "NRT", departure_date: Some("2026-09-03") },
                candidate("KLM", "KL861", 940.0),
                false,
            )
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "this trip has no segment 1",
            "a real trip with no such segment must not be reported as no such trip"
        );
    }

    #[test]
    fn adding_a_decided_option_marks_it_chosen_in_one_call() {
        // The ordinary path — "book me on this one" — must not need a second
        // call to say what it obviously meant.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "Japan", None, None).unwrap();
        let trip = store.add_segment(trip.id, None, "AMS", "NRT", "2026-09-03").unwrap();
        let trip = store
            .add_candidate(
                trip.id,
                1,
                ExpectedSegment { origin: "AMS", destination: "NRT", departure_date: Some("2026-09-03") },
                candidate("KLM", "KL861", 940.0),
                true,
            )
            .unwrap();
        assert!(trip.segments[0].candidates[0].chosen);
    }

    #[test]
    fn a_shifted_segment_keeps_its_own_options() {
        // segment_candidates is keyed by (trip_id, position), the same way
        // trip_segments is — so a shift that moved segments but not their
        // candidates would silently reattach somebody's chosen flight to a
        // different route while the trip still looked perfectly well-formed.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "Japan", None, None).unwrap();
        for (o, d, date) in [
            ("AMS", "NRT", "2026-09-03"),
            ("NRT", "OSA", "2026-09-10"),
            ("OSA", "AMS", "2026-09-17"),
        ] {
            store.add_segment(trip.id, None, o, d, date).unwrap();
        }
        // One chosen candidate per segment, identifiable by flight number.
        store
            .add_candidate(
                trip.id,
                1,
                ExpectedSegment { origin: "AMS", destination: "NRT", departure_date: Some("2026-09-03") },
                candidate("KLM", "KL861", 940.0),
                true,
            )
            .unwrap();
        store
            .add_candidate(
                trip.id,
                2,
                ExpectedSegment { origin: "NRT", destination: "OSA", departure_date: Some("2026-09-10") },
                candidate("ANA", "NH2001", 210.0),
                true,
            )
            .unwrap();
        store
            .add_candidate(
                trip.id,
                3,
                ExpectedSegment { origin: "OSA", destination: "AMS", departure_date: Some("2026-09-17") },
                candidate("KLM", "KL862", 980.0),
                true,
            )
            .unwrap();

        // Insert a new segment at position 1: AMS-NRT, NRT-OSA, OSA-AMS all
        // shift down one.
        let trip = store.add_segment(trip.id, Some(1), "AMS", "HEL", "2026-09-02").unwrap();
        let by_route: Vec<(String, String, Vec<String>)> = trip
            .segments
            .iter()
            .map(|s| {
                (
                    s.origin.clone(),
                    s.destination.clone(),
                    s.candidates.iter().map(|c| c.flight_numbers.clone()).collect(),
                )
            })
            .collect();
        assert_eq!(
            by_route,
            vec![
                ("AMS".to_string(), "HEL".to_string(), vec![]),
                ("AMS".to_string(), "NRT".to_string(), vec!["KL861".to_string()]),
                ("NRT".to_string(), "OSA".to_string(), vec!["NH2001".to_string()]),
                ("OSA".to_string(), "AMS".to_string(), vec!["KL862".to_string()]),
            ],
            "each segment's chosen flight must move with its own route, not stay pinned to its old position"
        );

        // Now drop a middle segment (NRT-OSA, now at position 3) and check
        // again: the remaining segments must still carry their own options.
        let trip = store.drop_segment(trip.id, 3).unwrap();
        let by_route: Vec<(String, String, Vec<String>)> = trip
            .segments
            .iter()
            .map(|s| {
                (
                    s.origin.clone(),
                    s.destination.clone(),
                    s.candidates.iter().map(|c| c.flight_numbers.clone()).collect(),
                )
            })
            .collect();
        assert_eq!(
            by_route,
            vec![
                ("AMS".to_string(), "HEL".to_string(), vec![]),
                ("AMS".to_string(), "NRT".to_string(), vec!["KL861".to_string()]),
                ("OSA".to_string(), "AMS".to_string(), vec!["KL862".to_string()]),
            ],
            "after closing the gap each surviving segment must still carry its own chosen flight, not a neighbour's"
        );
    }

    #[test]
    fn the_bounds_error_reads_correctly_with_exactly_one_segment() {
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "September", None, None).unwrap();
        store.add_segment(trip.id, None, "AMS", "LIS", "2026-09-03").unwrap();

        let err = store.add_segment(trip.id, Some(5), "LIS", "FCO", "2026-09-07").unwrap_err();
        assert_eq!(
            err.to_string(),
            "this trip has 1 segment, so position 5 is not somewhere to put one",
            "1 segment(s) reads wrong at one"
        );
    }

    #[test]
    fn add_candidate_refuses_when_the_route_or_date_no_longer_matches_what_was_checked() {
        // The caller validates a flight against a `Trip` it read earlier,
        // but that read and this write are two separate lock acquisitions —
        // a concurrent add_trip_segment or drop_trip_segment could
        // renumber positions in between. So the check has to run again
        // here, inside the same lock as the insert, against what the
        // caller says it validated rather than trusting the earlier read
        // to still be true.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "Japan", None, None).unwrap();
        let trip = store.add_segment(trip.id, None, "AMS", "NRT", "2026-09-03").unwrap();

        let err = store
            .add_candidate(
                trip.id,
                1,
                ExpectedSegment { origin: "AMS", destination: "LIS", departure_date: Some("2026-09-03") },
                candidate("KLM", "KL861", 940.0),
                false,
            )
            .unwrap_err();
        assert!(err.to_string().contains("AMS") && err.to_string().contains("NRT"), "got: {err}");

        let err = store
            .add_candidate(
                trip.id,
                1,
                ExpectedSegment { origin: "AMS", destination: "NRT", departure_date: Some("2026-09-05") },
                candidate("KLM", "KL861", 940.0),
                false,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("2026-09-03") && err.to_string().contains("2026-09-05"),
            "got: {err}"
        );

        // Matching values still insert.
        let trip = store
            .add_candidate(
                trip.id,
                1,
                ExpectedSegment { origin: "AMS", destination: "NRT", departure_date: Some("2026-09-03") },
                candidate("KLM", "KL861", 940.0),
                false,
            )
            .unwrap();
        assert_eq!(trip.segments[0].candidates.len(), 1);

        // No usable date to check is not the same as a checked mismatch.
        let trip = store
            .add_candidate(
                trip.id,
                1,
                ExpectedSegment { origin: "AMS", destination: "NRT", departure_date: None },
                candidate("ANA", "NH205", 900.0),
                false,
            )
            .unwrap();
        assert_eq!(trip.segments[0].candidates.len(), 2, "None means nothing to check, not a refusal");
    }

    #[test]
    fn choosing_an_option_after_the_trip_is_deleted_says_so_rather_than_no_such_option() {
        // choose_within only ever checks the segment_candidates row, which
        // a deleted trip has none of — indistinguishable, from there, from
        // a numbering mistake on a trip that still exists. choose_candidate
        // has to check the trip itself first so the two are told apart.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "Japan", None, None).unwrap();
        let trip = store.add_segment(trip.id, None, "AMS", "NRT", "2026-09-03").unwrap();
        store
            .add_candidate(
                trip.id,
                1,
                ExpectedSegment { origin: "AMS", destination: "NRT", departure_date: Some("2026-09-03") },
                candidate("KLM", "KL861", 940.0),
                false,
            )
            .unwrap();
        store.delete_trip(7, "Japan").unwrap();

        let err = store.choose_candidate(trip.id, 1, 1).unwrap_err();
        assert_eq!(err.to_string(), "no such trip", "a deleted trip must not read as a bad option number");
    }

    #[test]
    fn moving_a_segment_to_another_date_drops_the_options_it_invalidates() {
        // "Make it the 26th" is the commonest edit there is, and without it
        // the only way to honour one was to delete the trip and rebuild —
        // which is exactly what happened in production.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "Japan", None, None).unwrap();
        let trip = store.add_segment(trip.id, None, "HND", "AMS", "2026-09-27").unwrap();
        store
            .add_candidate(
                trip.id,
                1,
                ExpectedSegment {
                    origin: "HND",
                    destination: "AMS",
                    departure_date: Some("2026-09-27"),
                },
                candidate("China Southern", "CZ324,CZ307", 408.69),
                true,
            )
            .unwrap();

        let (trip, dropped, changed) =
            store.update_segment(trip.id, 1, None, None, Some("2026-09-26")).unwrap();
        assert!(changed);
        assert_eq!(trip.segments[0].departure_date, "2026-09-26");
        assert_eq!(dropped, 1, "an option for the 27th is not an option for the 26th");
        assert!(
            trip.segments[0].candidates.is_empty(),
            "keeping it would leave a flight bound to a day it does not fly"
        );
        assert_eq!(trip.segments[0].origin, "HND", "what was not asked for does not change");
    }

    #[test]
    fn a_segment_edit_that_changes_nothing_keeps_the_options() {
        // Restating the same date must not throw away work, the same way an
        // upsert that supplies nothing leaves a trip alone.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "Japan", None, None).unwrap();
        let trip = store.add_segment(trip.id, None, "HND", "AMS", "2026-09-27").unwrap();
        store
            .add_candidate(
                trip.id,
                1,
                ExpectedSegment {
                    origin: "HND",
                    destination: "AMS",
                    departure_date: Some("2026-09-27"),
                },
                candidate("China Southern", "CZ324,CZ307", 408.69),
                true,
            )
            .unwrap();

        let (trip, dropped, changed) =
            store.update_segment(trip.id, 1, Some("HND"), None, Some("2026-09-27")).unwrap();
        assert!(!changed, "nothing differed, so nothing was written");
        assert_eq!(dropped, 0);
        assert_eq!(trip.segments[0].candidates.len(), 1, "nothing changed, so nothing is lost");

        // And a position that does not exist is refused rather than ignored.
        assert!(store.update_segment(trip.id, 9, None, None, Some("2026-09-26")).is_err());
    }

    #[test]
    fn deleting_a_trip_takes_its_segments_and_options_with_it() {
        // Creating a trip is a side effect of a typo, so a typo needs an undo.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "Setpember", None, None).unwrap();
        let trip = store.add_segment(trip.id, None, "AMS", "LIS", "2026-09-03").unwrap();
        store
            .add_candidate(
                trip.id,
                1,
                ExpectedSegment { origin: "AMS", destination: "LIS", departure_date: Some("2026-09-03") },
                candidate("TAP", "TP675", 118.0),
                true,
            )
            .unwrap();

        assert!(store.delete_trip(7, "setpember").unwrap());
        assert!(store.find_trip(7, "Setpember").unwrap().is_none());
        assert!(!store.delete_trip(7, "Setpember").unwrap(), "deleting twice is not an error");

        // Another user's trip of the same name is untouched.
        store.upsert_trip(8, "Setpember", None, None).unwrap();
        assert!(!store.delete_trip(7, "Setpember").unwrap());
        assert!(store.find_trip(8, "Setpember").unwrap().is_some());
    }

    // ---- invite rounds, membership, waitlist ----

    #[test]
    fn a_round_admits_its_capacity_and_not_one_more() {
        let (s, _d) = test_store();
        assert!(s.create_round("autumn", 3).unwrap());
        for user in 1..=3 {
            assert_eq!(s.claim_seat(user, user, "autumn").unwrap(), Claim::Admitted);
        }
        assert_eq!(s.claim_seat(4, 4, "autumn").unwrap(), Claim::NoRoom);
        assert_eq!(s.active_members().unwrap(), vec![1, 2, 3]);

        let rounds = s.rounds().unwrap();
        assert_eq!(rounds.len(), 1);
        assert_eq!((rounds[0].used, rounds[0].capacity, rounds[0].open), (3, 3, true));
    }

    #[test]
    fn concurrent_claims_never_oversell() {
        // The whole reason claiming is one method: check-and-insert happens
        // under a single lock, so a rush on a link cannot overfill a round.
        let (s, _d) = test_store();
        assert!(s.create_round("rush", 5).unwrap());

        let admitted: usize = std::thread::scope(|scope| {
            let handles: Vec<_> = (1..=40)
                .map(|user| {
                    let s = s.clone();
                    scope.spawn(move || s.claim_seat(user, user, "rush").unwrap())
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .filter(|claim| *claim == Claim::Admitted)
                .count()
        });

        assert_eq!(admitted, 5, "a round of 5 admits exactly 5");
        assert_eq!(s.active_members().unwrap().len(), 5);
        assert_eq!(s.rounds().unwrap()[0].used, 5);
    }

    #[test]
    fn a_second_claim_by_the_same_person_spends_no_seat() {
        let (s, _d) = test_store();
        s.create_round("autumn", 2).unwrap();
        assert_eq!(s.claim_seat(1, 1, "autumn").unwrap(), Claim::Admitted);
        assert_eq!(s.claim_seat(1, 1, "autumn").unwrap(), Claim::AlreadyIn);
        // A member opening a *later* round's link is also already in.
        s.create_round("winter", 5).unwrap();
        assert_eq!(s.claim_seat(1, 1, "winter").unwrap(), Claim::AlreadyIn);

        assert_eq!(s.rounds().unwrap()[0].used, 1, "one person, one seat");
        assert_eq!(s.claim_seat(2, 2, "autumn").unwrap(), Claim::Admitted);
    }

    #[test]
    fn a_revoked_member_cannot_rejoin_through_any_link() {
        // Without this, revoking is theatre.
        let (s, _d) = test_store();
        s.create_round("autumn", 10).unwrap();
        s.claim_seat(1, 1, "autumn").unwrap();
        assert!(s.revoke(1).unwrap());
        assert!(s.active_members().unwrap().is_empty());

        assert_eq!(s.claim_seat(1, 1, "autumn").unwrap(), Claim::Revoked);
        s.create_round("winter", 10).unwrap();
        assert_eq!(s.claim_seat(1, 1, "winter").unwrap(), Claim::Revoked);

        // And being refused does not put them on the waitlist to be
        // announced back in later.
        assert_eq!(s.waiting_count().unwrap(), 0);
    }

    #[test]
    fn revoking_returns_no_seat_and_restoring_takes_none() {
        let (s, _d) = test_store();
        s.create_round("autumn", 2).unwrap();
        s.claim_seat(1, 1, "autumn").unwrap();
        s.claim_seat(2, 2, "autumn").unwrap();

        assert!(s.revoke(1).unwrap());
        assert_eq!(s.rounds().unwrap()[0].used, 2, "a round of 2 admitted 2 people, once");
        assert_eq!(s.claim_seat(3, 3, "autumn").unwrap(), Claim::NoRoom);

        assert!(s.restore(1).unwrap());
        assert_eq!(s.rounds().unwrap()[0].used, 2, "restoring consumes nothing either");
        assert_eq!(s.active_members().unwrap(), vec![1, 2]);

        // Neither is an error to repeat, and neither invents a member.
        assert!(!s.restore(1).unwrap(), "already restored");
        assert!(!s.revoke(999).unwrap(), "never a member");
        assert!(!s.restore(999).unwrap());
    }

    #[test]
    fn an_unknown_code_and_a_closed_round_are_refused_alike() {
        let (s, _d) = test_store();
        s.create_round("autumn", 10).unwrap();
        assert!(s.set_round_open("autumn", false).unwrap());

        assert_eq!(s.claim_seat(1, 1, "autumn").unwrap(), Claim::NoRoom);
        assert_eq!(s.claim_seat(2, 2, "no-such-round").unwrap(), Claim::NoRoom);
        assert!(s.active_members().unwrap().is_empty());
        // Both are people who tried to reach us, so both are queued.
        assert_eq!(s.waiting_count().unwrap(), 2);

        assert!(!s.set_round_open("no-such-round", true).unwrap());
    }

    #[test]
    fn a_reopened_round_admits_again() {
        let (s, _d) = test_store();
        s.create_round("autumn", 10).unwrap();
        s.set_round_open("autumn", false).unwrap();
        assert_eq!(s.claim_seat(1, 1, "autumn").unwrap(), Claim::NoRoom);

        assert!(s.set_round_open("autumn", true).unwrap());
        assert_eq!(s.claim_seat(1, 1, "autumn").unwrap(), Claim::Admitted);
    }

    #[test]
    fn a_round_name_cannot_be_reused() {
        // Otherwise two rounds pool their seats under one capacity.
        let (s, _d) = test_store();
        assert!(s.create_round("autumn", 10).unwrap());
        assert!(!s.create_round("autumn", 500).unwrap());
        assert_eq!(s.rounds().unwrap()[0].capacity, 10, "the first round is untouched");
    }

    #[test]
    fn being_turned_away_queues_you_and_getting_in_clears_it() {
        let (s, _d) = test_store();
        s.create_round("autumn", 1).unwrap();
        s.claim_seat(1, 1, "autumn").unwrap();

        assert_eq!(s.claim_seat(2, 8002, "autumn").unwrap(), Claim::NoRoom);
        assert_eq!(s.waitlist_to_invite().unwrap(), vec![(2, 8002)]);

        // A second attempt keeps their place rather than costing it.
        assert_eq!(s.claim_seat(2, 8002, "autumn").unwrap(), Claim::NoRoom);
        assert_eq!(s.waiting_count().unwrap(), 1);

        // Getting in through a later round takes them off the queue, so a
        // future announce does not chase somebody already inside.
        s.create_round("winter", 5).unwrap();
        assert_eq!(s.claim_seat(2, 8002, "winter").unwrap(), Claim::Admitted);
        assert_eq!(s.waiting_count().unwrap(), 0);
        assert!(s.waitlist_to_invite().unwrap().is_empty());
    }

    #[test]
    fn an_announce_stamps_only_the_rows_it_reached() {
        let (s, _d) = test_store();
        s.create_round("autumn", 0).unwrap();
        for user in [1, 2, 3] {
            s.claim_seat(user, 9000 + user, "autumn").unwrap();
        }
        assert_eq!(
            s.waitlist_to_invite().unwrap(),
            vec![(1, 9001), (2, 9002), (3, 9003)],
            "oldest first: if the next round is smaller than the queue, the \
             people who waited longest hear first"
        );

        // One reached, one unreachable, one that simply failed this time.
        s.mark_invited(1).unwrap();
        s.forget_waitlist(2).unwrap();

        assert_eq!(
            s.waitlist_to_invite().unwrap(),
            vec![(3, 9003)],
            "a re-run reaches only the people the first run missed"
        );
        assert_eq!(s.waiting_count().unwrap(), 1);
    }

    #[test]
    fn someone_turned_away_after_being_announced_to_is_queued_again() {
        // They pressed START on the new link and it was full too. They are
        // still waiting, so the next announce has to reach them.
        let (s, _d) = test_store();
        s.create_round("autumn", 0).unwrap();
        s.claim_seat(1, 9001, "autumn").unwrap();
        s.mark_invited(1).unwrap();
        assert!(s.waitlist_to_invite().unwrap().is_empty());

        s.create_round("winter", 0).unwrap();
        assert_eq!(s.claim_seat(1, 9001, "winter").unwrap(), Claim::NoRoom);
        assert_eq!(s.waitlist_to_invite().unwrap(), vec![(1, 9001)]);
    }

    #[test]
    fn the_daily_cap_counts_messages_and_only_todays() {
        let (s, _d) = test_store();
        let today = chrono::Utc::now().date_naive();
        let yesterday = today - chrono::Duration::days(1);

        s.log_request(1, "text").unwrap();
        s.log_request(1, "photo").unwrap();
        // Not requests: a reaction is not one, and a flight search is a
        // sub-event of a message that was already counted.
        s.log_request(1, "reaction").unwrap();
        s.log_request(1, Store::FLIGHT_SEARCH).unwrap();
        // Another user's traffic is not this user's.
        s.log_request(2, "text").unwrap();
        // Yesterday is spent.
        s.log_request_at(1, "text", &format!("{yesterday} 23:59:59")).unwrap();

        assert_eq!(s.requests_today(1).unwrap(), 2);
        assert_eq!(s.requests_today(2).unwrap(), 1);
        assert_eq!(s.requests_today(999).unwrap(), 0);
    }
}
