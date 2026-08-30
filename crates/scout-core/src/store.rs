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
    account_id BIGINT NOT NULL,
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
    account_id BIGINT NOT NULL,
    channel TEXT NOT NULL DEFAULT 'telegram',
    address TEXT NOT NULL,
    item TEXT NOT NULL,
    interval_days BIGINT NOT NULL,
    next_due TEXT NOT NULL,
    active BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE TABLE IF NOT EXISTS user_facts (
    account_id BIGINT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (account_id, key)
);
CREATE TABLE IF NOT EXISTS request_log (
    account_id BIGINT NOT NULL,
    kind TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
-- Telegram display names, refreshed on every request so /stat can label a
-- user id with something readable. Names change; the id is the identity.
CREATE TABLE IF NOT EXISTS users (
    account_id BIGINT PRIMARY KEY,
    display_name TEXT NOT NULL,
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
-- One row per person admitted, ever. `account_id` is the key, so a person
-- belongs to one round; `revoked_at` set means removed, and the row stays
-- put so the seat is not handed back.
CREATE TABLE IF NOT EXISTS members (
    account_id    BIGINT PRIMARY KEY,
    code       TEXT NOT NULL,
    joined_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    revoked_at TIMESTAMP
);
-- People a full or unknown round turned away. Where to reach them lives in
-- `deliveries`, because the START they pressed is the permission to do it.
CREATE TABLE IF NOT EXISTS waitlist (
    account_id BIGINT PRIMARY KEY,
    code       TEXT NOT NULL,
    seen_at    TIMESTAMP NOT NULL DEFAULT current_timestamp,
    invited_at TIMESTAMP
);
-- A named plan. The itinerary is durable; prices are not, so nothing here
-- holds an offer id — see the trips design doc.
CREATE SEQUENCE IF NOT EXISTS trips_id_seq;
CREATE TABLE IF NOT EXISTS trips (
    id          BIGINT PRIMARY KEY DEFAULT nextval('trips_id_seq'),
    account_id     BIGINT NOT NULL,
    name        TEXT NOT NULL,
    -- lowercased `name`: the trip is addressed by what the traveller calls
    -- it, and "September" and "september" are the same trip.
    name_key    TEXT NOT NULL,
    adults      BIGINT NOT NULL DEFAULT 1,
    cabin_class TEXT,
    status      TEXT NOT NULL DEFAULT 'planning',
    created_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    UNIQUE (account_id, name_key)
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
-- Magic-link tokens. A row rather than a signed value because a link must
-- be single-use: a replayable one is a standing account key sitting in an
-- inbox. Sign-in is rare, so the store mutex is cheap here in a way it
-- would not be on a per-request session check.
CREATE TABLE IF NOT EXISTS login_tokens (
    token_hash  TEXT PRIMARY KEY,
    email       TEXT NOT NULL,
    -- Set when linking an address to an account that is already signed in;
    -- NULL when the link is a sign-in and the account is not known yet.
    account_id  BIGINT,
    expires_at  TIMESTAMP NOT NULL,
    -- Kept rather than deleted, so "already used" and "expired" stay
    -- distinguishable — they call for different advice.
    consumed_at TIMESTAMP
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
    pub account_id: i64,
    #[serde(skip)]
    pub channel: String,
    #[serde(skip)]
    pub address: String,
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

/// What linking a second way of signing in did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkOutcome {
    Linked,
    AlreadyYours,
    /// Somebody else proved this identity first. Never resolved by moving
    /// it: two sign-ups stay two accounts until a human decides otherwise,
    /// and a wrong merge cannot be undone.
    TakenByAnother,
}

/// What a magic link turned out to be worth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenOutcome {
    Valid { email: String, account_id: Option<i64> },
    Expired,
    AlreadyUsed,
    Unknown,
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
    Code(fn(&Connection) -> Result<()>),
}

/// The tables phase one introduced. Included in `MIGRATIONS` as well, so a
/// brand-new database is created in the finished shape; kept as a step so an
/// existing one still gets them.
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

/// Every Telegram id that appears anywhere in the pre-phase-one schema.
/// `user_chats` is included even though it is about to be replaced: someone
/// might appear there and nowhere else.
const LEGACY_USER_IDS: &str = "
SELECT user_id FROM purchases
UNION SELECT user_id FROM reminders
UNION SELECT user_id FROM user_facts
UNION SELECT user_id FROM request_log
UNION SELECT user_id FROM users
UNION SELECT user_id FROM user_chats
UNION SELECT user_id FROM members
UNION SELECT user_id FROM waitlist
UNION SELECT user_id FROM trips
";

/// One account per pre-existing Telegram id.
///
/// Written in Rust rather than SQL because each row needs `nextval` and the
/// id it produced, and DuckDB has no `setval` to fix a sequence up
/// afterwards.
fn step_2_backfill_accounts(conn: &Connection) -> Result<()> {
    let sql = format!("SELECT DISTINCT user_id FROM ({LEGACY_USER_IDS}) ORDER BY user_id");
    let mut stmt = conn.prepare(&sql)?;
    let ids: Vec<i64> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    drop(stmt);
    for user_id in ids {
        let account_id: i64 = conn.query_row(
            "INSERT INTO accounts (id) VALUES (nextval('accounts_id_seq')) RETURNING id",
            [],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO identities (account_id, kind, external_id) VALUES (?, 'telegram', ?)",
            params![account_id, user_id.to_string()],
        )?;
    }
    Ok(())
}

/// Every table is rebuilt rather than altered in place, even where the
/// column is unconstrained and `ALTER` would have worked.
///
/// Measured: DuckDB refuses to `ALTER` and then `UPDATE` the same table
/// inside one transaction — "Attempting to modify table purchases but
/// another transaction has altered this table". Doing it in autocommit
/// works but gives up atomicity, and this migration is one-way over live
/// purchase history. A rebuild touches the new table and the old one
/// separately, so it commits as a unit.
///
/// `reminders` gains its `channel` and `address` here rather than in a later
/// step; the table is being rewritten anyway and two rebuilds would be waste.
const STEP_3_UNCONSTRAINED: &str = r#"
CREATE TABLE purchases_new (
    id BIGINT PRIMARY KEY DEFAULT nextval('purchases_id_seq'),
    account_id BIGINT NOT NULL,
    item TEXT NOT NULL,
    store TEXT NOT NULL,
    url TEXT,
    price DOUBLE,
    currency TEXT,
    notes TEXT,
    purchased_at TEXT,
    recorded_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
INSERT INTO purchases_new
    (id, account_id, item, store, url, price, currency, notes, purchased_at, recorded_at)
SELECT p.id, i.account_id, p.item, p.store, p.url, p.price, p.currency, p.notes,
       p.purchased_at, p.recorded_at
FROM purchases p
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(p.user_id AS TEXT);
DROP TABLE purchases;
ALTER TABLE purchases_new RENAME TO purchases;

CREATE TABLE reminders_new (
    id BIGINT PRIMARY KEY DEFAULT nextval('reminders_id_seq'),
    account_id BIGINT NOT NULL,
    channel TEXT NOT NULL DEFAULT 'telegram',
    address TEXT NOT NULL,
    item TEXT NOT NULL,
    interval_days BIGINT NOT NULL,
    next_due TEXT NOT NULL,
    active BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
INSERT INTO reminders_new
    (id, account_id, channel, address, item, interval_days, next_due, active, created_at)
SELECT r.id, i.account_id, 'telegram', CAST(r.chat_id AS TEXT), r.item,
       r.interval_days, r.next_due, r.active, r.created_at
FROM reminders r
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(r.user_id AS TEXT);
DROP TABLE reminders;
ALTER TABLE reminders_new RENAME TO reminders;

CREATE TABLE request_log_new (
    account_id BIGINT NOT NULL,
    kind TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
INSERT INTO request_log_new (account_id, kind, created_at)
SELECT i.account_id, l.kind, l.created_at
FROM request_log l
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(l.user_id AS TEXT);
DROP TABLE request_log;
ALTER TABLE request_log_new RENAME TO request_log;
"#;

/// The five tables whose `account_id` sits inside a PK or UNIQUE constraint.
/// DuckDB will not drop such a column at all, so a rebuild is the only
/// route — and it is the same route step 3 takes, for atomicity.
const STEP_4_REBUILDS: &str = r#"
-- Do this FIRST, before waitlist is rebuilt without its chat_id. Someone on
-- the waitlist may never have had a user_chats row, and the START they
-- pressed is the only permission we have to message them. Dropping the
-- column before reading it would lose that silently, and the next announce
-- would simply skip them.
INSERT INTO deliveries (account_id, channel, address)
SELECT i.account_id, 'telegram', CAST(w.chat_id AS TEXT)
FROM waitlist w
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(w.user_id AS TEXT)
ON CONFLICT (account_id, channel) DO NOTHING;

CREATE TABLE user_facts_new (
    account_id BIGINT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (account_id, key)
);
INSERT INTO user_facts_new (account_id, key, value, updated_at)
SELECT i.account_id, f.key, f.value, f.updated_at FROM user_facts f
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(f.user_id AS TEXT);
DROP TABLE user_facts;
ALTER TABLE user_facts_new RENAME TO user_facts;

CREATE TABLE users_new (
    account_id   BIGINT PRIMARY KEY,
    display_name TEXT NOT NULL,
    updated_at   TIMESTAMP NOT NULL DEFAULT current_timestamp
);
INSERT INTO users_new (account_id, display_name, updated_at)
SELECT i.account_id, u.display_name, u.updated_at FROM users u
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(u.user_id AS TEXT);
DROP TABLE users;
ALTER TABLE users_new RENAME TO users;

CREATE TABLE members_new (
    account_id BIGINT PRIMARY KEY,
    code       TEXT NOT NULL,
    joined_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    revoked_at TIMESTAMP
);
INSERT INTO members_new (account_id, code, joined_at, revoked_at)
SELECT i.account_id, m.code, m.joined_at, m.revoked_at FROM members m
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(m.user_id AS TEXT);
DROP TABLE members;
ALTER TABLE members_new RENAME TO members;

CREATE TABLE waitlist_new (
    account_id BIGINT PRIMARY KEY,
    code       TEXT NOT NULL,
    seen_at    TIMESTAMP NOT NULL DEFAULT current_timestamp,
    invited_at TIMESTAMP
);
INSERT INTO waitlist_new (account_id, code, seen_at, invited_at)
SELECT i.account_id, w.code, w.seen_at, w.invited_at FROM waitlist w
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(w.user_id AS TEXT);
DROP TABLE waitlist;
ALTER TABLE waitlist_new RENAME TO waitlist;

CREATE TABLE trips_new (
    id          BIGINT PRIMARY KEY DEFAULT nextval('trips_id_seq'),
    account_id  BIGINT NOT NULL,
    name        TEXT NOT NULL,
    name_key    TEXT NOT NULL,
    adults      BIGINT NOT NULL DEFAULT 1,
    cabin_class TEXT,
    status      TEXT NOT NULL DEFAULT 'planning',
    created_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    UNIQUE (account_id, name_key)
);
INSERT INTO trips_new
    (id, account_id, name, name_key, adults, cabin_class, status, created_at, updated_at)
SELECT t.id, i.account_id, t.name, t.name_key, t.adults, t.cabin_class, t.status,
       t.created_at, t.updated_at
FROM trips t
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(t.user_id AS TEXT);
DROP TABLE trips;
ALTER TABLE trips_new RENAME TO trips;
"#;

/// `user_chats` was already "where to reach this person", which is what
/// `deliveries` is. The waitlist rescue happened at the top of step 4,
/// before that column was dropped; this is the richer record and overwrites
/// it where both exist, because user_chats is the more recent sighting.
const STEP_5_DELIVERIES: &str = r#"
-- `updated_at` is carried from the source row rather than set to now:
-- bare `current_timestamp` inside DO UPDATE SET is parsed as a column
-- reference by DuckDB, and the real last-seen time is the truer value.
INSERT INTO deliveries (account_id, channel, address, updated_at)
SELECT i.account_id, 'telegram', CAST(c.chat_id AS TEXT), c.updated_at
FROM user_chats c
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(c.user_id AS TEXT)
ON CONFLICT (account_id, channel)
DO UPDATE SET address = excluded.address, updated_at = excluded.updated_at;

DROP TABLE user_chats;
"#;

const STEP_6_LOGIN_TOKENS: &str = r#"
CREATE TABLE IF NOT EXISTS login_tokens (
    token_hash  TEXT PRIMARY KEY,
    email       TEXT NOT NULL,
    account_id  BIGINT,
    expires_at  TIMESTAMP NOT NULL,
    consumed_at TIMESTAMP
);
"#;

fn steps() -> Vec<(i64, Step)> {
    vec![
        (1, Step::Sql(STEP_1_NEW_TABLES)),
        (2, Step::Code(step_2_backfill_accounts)),
        (3, Step::Sql(STEP_3_UNCONSTRAINED)),
        (4, Step::Sql(STEP_4_REBUILDS)),
        (5, Step::Sql(STEP_5_DELIVERIES)),
        (6, Step::Sql(STEP_6_LOGIN_TOKENS)),
    ]
}

/// True when this database predates phase one. `purchases.user_id` is the
/// marker: `MIGRATIONS` has created that table with `account_id` since, so
/// the old column name can only survive on a file that already existed.
fn legacy_shape(conn: &Connection) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM information_schema.columns
         WHERE table_name = 'purchases' AND column_name = 'user_id'",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

fn apply_steps(conn: &Connection, db_path: &Path) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version BIGINT NOT NULL)")?;
    let mut stmt = conn.prepare("SELECT version FROM schema_version")?;
    let current: Option<i64> = stmt.query_map([], |r| r.get(0))?.next().transpose()?;
    drop(stmt);
    let mut current = match current {
        Some(v) => v,
        None => {
            // No recorded version yet, which is true of two very different
            // databases: one created before phase one, and one created just
            // now by `MIGRATIONS` in the finished shape. Telling them apart
            // matters — the upgrade steps read `purchases.user_id`, which
            // only the older one has, and would fail on a fresh file.
            let start = if legacy_shape(conn)? {
                0
            } else {
                steps().last().map(|(n, _)| *n).unwrap_or(0)
            };
            conn.execute("INSERT INTO schema_version (version) VALUES (?)", params![start])?;
            start
        }
    };
    // Before the first step, not after: a migration cannot be undone, so this
    // is the last moment the old shape still exists.
    //
    // A failure here is logged and the migration proceeds anyway. That is
    // deliberate and it is sharp — it means an irreversible change can run
    // unprotected. The alternative, refusing to start, turns a full disk into
    // a bot that will not boot, at the worst possible moment. Reversing this
    // choice is one `?`; see the design doc.
    let target = steps().last().map(|(n, _)| *n).unwrap_or(0);
    if target > current {
        let dir = crate::backup::dir_for(db_path);
        let taken = std::fs::create_dir_all(&dir)
            .map_err(anyhow::Error::from)
            .and_then(|()| {
                let to = dir.join(crate::backup::file_name_now(
                    crate::backup::Reason::Migration { to: target },
                ));
                backup_connection(conn, &to).map(|()| to)
            });
        match taken {
            Ok(to) => tracing::info!(path = %to.display(), from = current, to = target,
                "backed up before migrating"),
            Err(e) => tracing::error!(error = %e, from = current, to = target,
                "COULD NOT BACK UP BEFORE MIGRATING; proceeding anyway"),
        }
    }

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

/// Writes a consistent copy of `conn`'s database to `path`.
///
/// DuckDB is single-writer, so this is the only way to get a copy that is not
/// merely crash-consistent: it runs on the connection that already holds the
/// database open, folding in whatever is still sitting in the write-ahead log.
/// A copy taken from outside — `cp`, a volume snapshot, a provider's block
/// backup — captures whatever was on disk mid-flight and relies on WAL replay,
/// exactly like recovering from a power cut.
///
/// Written to a `.partial` and renamed, so an interrupted backup leaves
/// something obviously unfinished rather than something that looks restorable.
fn backup_connection(conn: &Connection, path: &Path) -> Result<()> {
    // The source's identifier is derived from its filename — `scout` in
    // production, a random temp name under test — so it is asked for rather
    // than assumed. Hardcoding it would pass no test and quietly couple
    // production to a filename.
    let source: String = conn.query_row("SELECT current_database()", [], |r| r.get(0))?;
    let partial = path.with_extension("partial");
    let _ = std::fs::remove_file(&partial);

    conn.execute_batch(&format!(
        "ATTACH '{}' AS scout_backup; COPY FROM DATABASE \"{}\" TO scout_backup; DETACH scout_backup;",
        partial.display(),
        source,
    ))?;
    std::fs::rename(&partial, path)?;
    Ok(())
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref())?;
        conn.execute_batch(MIGRATIONS)?;
        apply_steps(&conn, path.as_ref())?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// A consistent copy of this database, taken without stopping anything.
    ///
    /// Holds the store's mutex for the duration, which blocks the agent. At
    /// this database's size that is imperceptible; it is worth knowing because
    /// it scales with the file.
    pub(crate) fn backup_to(&self, path: &Path) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        backup_connection(&conn, path)
    }

    /// Highest migration step applied to this database.
    pub fn schema_version(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT version FROM schema_version", [], |r| r.get(0))?)
    }

    pub fn record_purchase(&self, account_id: i64, p: NewPurchase) -> Result<Purchase> {
        let conn = self.conn.lock().unwrap();
        let id: i64 = conn.query_row(
            "INSERT INTO purchases (account_id, item, store, url, price, currency, notes, purchased_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
            params![account_id, p.item, p.store, p.url, p.price, p.currency, p.notes, p.purchased_at],
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
        account_id: i64,
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
                    "{SELECT} WHERE account_id = ? AND (lower(item) LIKE ? \
                     OR lower(store) LIKE ? OR lower(coalesce(notes, '')) LIKE ?) {ORDER}"
                );
                let mut stmt = conn.prepare(&sql)?;
                let rows =
                    stmt.query_map(params![account_id, like, like, like, limit as i64], row_to_purchase)?;
                for row in rows {
                    out.push(row?);
                }
            }
            None => {
                let sql = format!("{SELECT} WHERE account_id = ? {ORDER}");
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params![account_id, limit as i64], row_to_purchase)?;
                for row in rows {
                    out.push(row?);
                }
            }
        }
        Ok(out)
    }

    /// `address` is where this reminder should be delivered — the chat it
    /// was created in, not wherever the account was last seen. A reminder
    /// set in a group belongs to that group.
    pub fn create_reminder(
        &self,
        account_id: i64,
        channel: &str,
        address: &str,
        item: &str,
        interval_days: i64,
        next_due: &str,
    ) -> Result<Reminder> {
        let conn = self.conn.lock().unwrap();
        let id: i64 = conn.query_row(
            "INSERT INTO reminders (account_id, channel, address, item, interval_days, next_due)
             VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
            params![account_id, channel, address, item, interval_days, next_due],
            |row| row.get(0),
        )?;
        Ok(Reminder {
            id,
            account_id,
            channel: channel.to_string(),
            address: address.to_string(),
            item: item.to_string(),
            interval_days,
            next_due: next_due.to_string(),
        })
    }

    /// Active reminders for one user, soonest first.
    pub fn list_reminders(&self, account_id: i64) -> Result<Vec<Reminder>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, account_id, channel, address, item, interval_days, next_due FROM reminders
             WHERE account_id = ? AND active ORDER BY next_due ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![account_id], row_to_reminder)?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Returns true if an active reminder belonging to this user was cancelled.
    pub fn cancel_reminder(&self, account_id: i64, id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE reminders SET active = false WHERE id = ? AND account_id = ? AND active",
            params![id, account_id],
        )?;
        Ok(n > 0)
    }

    /// All users' active reminders with next_due <= today (ISO date string).
    pub fn due_reminders(&self, today: &str) -> Result<Vec<Reminder>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, account_id, channel, address, item, interval_days, next_due FROM reminders
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
    pub fn upsert_fact(&self, account_id: i64, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO user_facts (account_id, key, value) VALUES (?, ?, ?)
             ON CONFLICT (account_id, key)
             DO UPDATE SET value = excluded.value, updated_at = now()",
            params![account_id, key, value],
        )?;
        Ok(())
    }

    /// One user's profile facts as (key, value), sorted by key.
    pub fn list_facts(&self, account_id: i64) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT key, value FROM user_facts WHERE account_id = ? ORDER BY key ASC")?;
        let rows = stmt.query_map(params![account_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Returns true if the fact existed and was removed.
    pub fn forget_fact(&self, account_id: i64, key: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM user_facts WHERE account_id = ? AND key = ?",
            params![account_id, key],
        )?;
        Ok(n > 0)
    }

    /// Record one handled request for usage statistics.
    /// `request_log.kind` for one billable Duffel search. Kept here beside
    /// the table it is written into, because `/stat` reads it back by name
    /// and a typo on either side would silently report zero.
    pub const FLIGHT_SEARCH: &'static str = "flight_search";

    pub fn log_request(&self, account_id: i64, kind: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO request_log (account_id, kind) VALUES (?, ?)",
            params![account_id, kind],
        )?;
        Ok(())
    }

    /// Remember what to call a user id in `/stat`. Deliberately separate
    /// from `log_request`: commands should teach the bot your name without
    /// also counting as requests. A blank name is not a name.
    pub fn remember_user(&self, account_id: i64, display_name: &str) -> Result<()> {
        let name = display_name.trim();
        if name.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO users (account_id, display_name) VALUES (?, ?)
             ON CONFLICT (account_id)
             DO UPDATE SET display_name = excluded.display_name, updated_at = now()",
            params![account_id, name],
        )?;
        Ok(())
    }

    #[cfg(test)]
    fn log_request_at(&self, account_id: i64, kind: &str, at: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO request_log (account_id, kind, created_at) VALUES (?, ?, CAST(? AS TIMESTAMP))",
            params![account_id, kind, at],
        )?;
        Ok(())
    }

    /// Per-day request counts scoped to a single user, as
    /// (account_id, day "YYYY-MM-DD", count) at or after `cutoff`
    /// ("YYYY-MM-DD 00:00:00"). This is what non-admin `/stat` callers get,
    /// so they only ever see their own volume however many users share the
    /// bot.
    pub fn usage_stats_for(&self, cutoff: &str, account_id: i64) -> Result<Vec<(i64, String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT account_id, strftime(created_at, '%Y-%m-%d') AS day, count(*)
             FROM request_log WHERE account_id = ? AND created_at >= CAST(? AS TIMESTAMP)
             GROUP BY account_id, day ORDER BY day ASC, account_id ASC",
        )?;
        let rows = stmt.query_map(params![account_id, cutoff], |row| {
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
            "SELECT account_id, strftime(created_at, '%Y-%m-%d') AS day, count(*)
             FROM request_log WHERE created_at >= CAST(? AS TIMESTAMP)
             GROUP BY account_id, day ORDER BY day ASC, account_id ASC",
        )?;
        let rows = stmt.query_map(params![cutoff], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// The most recent conversation for this scope, and whether it has gone
    /// quiet for longer than `ttl_secs`. Creates nothing — whether an
    /// aged-out thread is resumed or replaced is a judgement the store has
    /// no business making.
    pub fn latest_conversation(
        &self,
        account_id: i64,
        scope: &str,
        ttl_secs: i64,
    ) -> Result<Option<(i64, bool)>> {
        let conn = self.conn.lock().unwrap();
        // The cast is load-bearing: `current_timestamp` is TIMESTAMPTZ and
        // DuckDB has no `TIMESTAMPTZ - INTERVAL` overload.
        let mut stmt = conn.prepare(
            "SELECT id,
                    updated_at <= CAST(current_timestamp AS TIMESTAMP) - to_seconds(?)
             FROM conversations WHERE account_id = ? AND scope = ?
             ORDER BY updated_at DESC LIMIT 1",
        )?;
        let row: Option<(i64, bool)> = stmt
            .query_map(params![ttl_secs, account_id, scope], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .next()
            .transpose()?;
        Ok(row)
    }

    pub fn start_conversation(&self, account_id: i64, scope: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "INSERT INTO conversations (id, account_id, scope)
             VALUES (nextval('conversations_id_seq'), ?, ?) RETURNING id",
            params![account_id, scope],
            |r| r.get(0),
        )?)
    }

    /// Marks a conversation as spoken in, so its TTL runs from now.
    pub fn touch_conversation(&self, conversation_id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE conversations SET updated_at = now() WHERE id = ?",
            params![conversation_id],
        )?;
        Ok(())
    }

    /// The last `limit` messages, oldest first — the order a provider wants.
    pub fn conversation_messages(&self, conversation_id: i64, limit: usize) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT body FROM (
                 SELECT body, position FROM messages WHERE conversation_id = ?
                 ORDER BY position DESC LIMIT ?
             ) ORDER BY position ASC",
        )?;
        let rows = stmt.query_map(params![conversation_id, limit as i64], |r| r.get(0))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Replaces a conversation's messages wholesale, in one lock so a reader
    /// never sees the thread half-written.
    ///
    /// A rewrite rather than an append because `trim_history` drops messages
    /// from the front: what is stored has to be what the agent will actually
    /// be sent next time, not a growing log that disagrees with it.
    pub fn replace_messages(&self, conversation_id: i64, bodies: &[String]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<()> {
            conn.execute(
                "DELETE FROM messages WHERE conversation_id = ?",
                params![conversation_id],
            )?;
            for (i, body) in bodies.iter().enumerate() {
                conn.execute(
                    "INSERT INTO messages (id, conversation_id, position, body)
                     VALUES (nextval('messages_id_seq'), ?, ?, ?)",
                    params![conversation_id, i as i64, body],
                )?;
            }
            conn.execute(
                "UPDATE conversations SET updated_at = now() WHERE id = ?",
                params![conversation_id],
            )?;
            Ok(())
        })();
        if let Err(e) = result {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
        conn.execute_batch("COMMIT")?;
        Ok(())
    }

    /// The account proving control of this identity, creating one if the
    /// identity is new.
    ///
    /// `kind` is `&'static str` rather than `&str` on purpose. It is half of
    /// a primary key, and a kind read off the wire — a typo, or a value an
    /// attacker chose — would silently open a parallel identity space that
    /// nothing else can see.
    ///
    /// Both branches run under the same lock, so two updates arriving from
    /// the same person cannot mint two accounts for them.
    pub fn account_for_identity(&self, kind: &'static str, external_id: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT account_id FROM identities WHERE kind = ? AND external_id = ?")?;
        let found: Option<i64> =
            stmt.query_map(params![kind, external_id], |r| r.get(0))?.next().transpose()?;
        drop(stmt);
        if let Some(id) = found {
            return Ok(id);
        }
        let account_id: i64 = conn.query_row(
            "INSERT INTO accounts (id) VALUES (nextval('accounts_id_seq')) RETURNING id",
            [],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO identities (account_id, kind, external_id) VALUES (?, ?, ?)",
            params![account_id, kind, external_id],
        )?;
        Ok(account_id)
    }

    /// Attaches a second identity to an account that already exists.
    ///
    /// The `PRIMARY KEY (kind, external_id)` is what actually prevents two
    /// owners under a race; this read exists to produce a sentence a person
    /// can act on instead of a constraint violation.
    pub fn link_identity(
        &self,
        account_id: i64,
        kind: &'static str,
        external_id: &str,
    ) -> Result<LinkOutcome> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT account_id FROM identities WHERE kind = ? AND external_id = ?")?;
        let owner: Option<i64> =
            stmt.query_map(params![kind, external_id], |r| r.get(0))?.next().transpose()?;
        drop(stmt);
        match owner {
            Some(id) if id == account_id => return Ok(LinkOutcome::AlreadyYours),
            Some(_) => return Ok(LinkOutcome::TakenByAnother),
            None => {}
        }
        conn.execute(
            "INSERT INTO identities (account_id, kind, external_id) VALUES (?, ?, ?)",
            params![account_id, kind, external_id],
        )?;
        Ok(LinkOutcome::Linked)
    }

    /// Which ways of proving this account exist — `'email'`, `'telegram'`.
    ///
    /// The kinds and never the external ids. What asks is a page offering
    /// to attach whichever method is missing, and it needs to know that a
    /// method exists, not what it is. Handing back the address as well
    /// would put a value somebody chose into a page, and then escaping it
    /// correctly would be everyone's problem forever.
    pub fn identity_kinds(&self, account_id: i64) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT kind FROM identities WHERE account_id = ? ORDER BY kind ASC")?;
        let rows = stmt.query_map(params![account_id], |row| row.get(0))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Whether this account holds a seat that has not been revoked.
    ///
    /// The same question `claim_seat` asks first, as a read. A page that
    /// reported standing by calling `claim_seat` would seat a queued
    /// visitor the moment they looked at it, and spend a seat on a `GET`.
    pub fn is_member(&self, account_id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT revoked_at IS NULL FROM members WHERE account_id = ?")?;
        let standing: Option<bool> =
            stmt.query_map(params![account_id], |row| row.get(0))?.next().transpose()?;
        Ok(standing.unwrap_or(false))
    }

    /// Records a token that has been mailed out.
    ///
    /// `token_hash` is a hash of the value in the link, never the value:
    /// a database that leaks must not hand over working sign-in links.
    /// `ttl_secs` is signed so a test can issue one already expired.
    ///
    /// `expires_at` is computed in Rust from `chrono::Utc::now()` — the same
    /// UTC-naive clock `consume_login_token` compares against via
    /// `current_timestamp AT TIME ZONE 'UTC'` — rather than derived with
    /// bare `current_timestamp`, which is local. See `requests_today` for
    /// the bug that pattern caused.
    ///
    /// The addition happens here rather than as `TIMESTAMP + INTERVAL` in
    /// SQL on purpose: on a freshly opened, file-backed connection (unlike
    /// an in-memory one, and unlike an ad-hoc `query_row` probe run after
    /// other queries have already primed the process) DuckDB's binder can
    /// fail that expression with "No function matches ... (TIMESTAMP,
    /// INTERVAL)" — a startup race in this build, not a real type error.
    /// Binding an already-computed timestamp sidesteps it; the plain `<`
    /// comparison below over two TIMESTAMP values needs no such arithmetic
    /// and does not carry the same risk.
    pub fn issue_login_token(
        &self,
        token_hash: &str,
        email: &str,
        account_id: Option<i64>,
        ttl_secs: i64,
    ) -> Result<()> {
        let expires_at = chrono::Utc::now().naive_utc() + chrono::Duration::seconds(ttl_secs);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO login_tokens (token_hash, email, account_id, expires_at)
             VALUES (?, ?, ?, ?)",
            params![token_hash, email, account_id, expires_at.to_string()],
        )?;
        Ok(())
    }

    /// Spends a token, if it has anything left to spend.
    ///
    /// Marking consumed and reading the row happen under one mutex, so two
    /// simultaneous clicks cannot both come back `Valid`.
    pub fn consume_login_token(&self, token_hash: &str) -> Result<TokenOutcome> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT email, account_id, consumed_at IS NOT NULL,
                    expires_at < (current_timestamp AT TIME ZONE 'UTC')::TIMESTAMP
             FROM login_tokens WHERE token_hash = ?",
        )?;
        let row: Option<(String, Option<i64>, bool, bool)> = stmt
            .query_map(params![token_hash], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .next()
            .transpose()?;
        drop(stmt);
        let Some((email, account_id, consumed, expired)) = row else {
            return Ok(TokenOutcome::Unknown);
        };
        if consumed {
            return Ok(TokenOutcome::AlreadyUsed);
        }
        if expired {
            return Ok(TokenOutcome::Expired);
        }
        conn.execute(
            "UPDATE login_tokens SET consumed_at = current_timestamp WHERE token_hash = ?",
            params![token_hash],
        )?;
        Ok(TokenOutcome::Valid { email, account_id })
    }

    /// The account behind a Telegram user, creating one the first time.
    ///
    /// Delegates to `account_for_identity` so there is one lookup-or-create
    /// implementation rather than two that can drift.
    pub fn account_for_telegram(&self, telegram_id: i64) -> Result<i64> {
        self.account_for_identity("telegram", &telegram_id.to_string())
    }

    /// Records where this person last spoke, for announcements.
    ///
    /// A user id and a chat id are the same number in a private chat and
    /// different in a group, so the chat is stored rather than derived.
    pub fn note_delivery(&self, account_id: i64, channel: &str, address: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO deliveries (account_id, channel, address) VALUES (?, ?, ?)
             ON CONFLICT (account_id, channel)
             DO UPDATE SET address = excluded.address, updated_at = now()",
            params![account_id, channel, address],
        )?;
        Ok(())
    }

    /// Everyone the bot could announce something to on a channel, as
    /// (account, address).
    pub fn broadcast_targets(&self) -> Result<Vec<(i64, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT account_id, CAST(address AS BIGINT) FROM deliveries
             WHERE channel = 'telegram' ORDER BY account_id ASC",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Everyone admitted and not since revoked, as **Telegram ids**. Read
    /// once at startup to build the membership set the gate consults.
    ///
    /// Telegram ids rather than account ids because the gate runs on every
    /// update and all it has is `sender_id`. Resolving an account there
    /// would put a database read in front of every message from every
    /// stranger, which is the cost this set exists to avoid.
    pub fn active_members(&self) -> Result<Vec<i64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT CAST(i.external_id AS BIGINT)
             FROM members m
             JOIN identities i ON i.account_id = m.account_id AND i.kind = 'telegram'
             WHERE m.revoked_at IS NULL
             ORDER BY 1 ASC",
        )?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Try to take a seat in `code` for `account_id`.
    ///
    /// One method, one lock acquisition, so check-and-insert is atomic by
    /// construction: a round of 100 admits exactly 100 however many people
    /// press START at the same moment. There is no counter to drift from
    /// the rows — seats used *is* `count(*)` over them.
    pub fn claim_seat(&self, account_id: i64, code: &str) -> Result<Claim> {
        let conn = self.conn.lock().unwrap();

        // Membership decides before the round does. Checking the round
        // first would let a revoked person be told "that round is full",
        // and a member re-clicking a link consume a second seat.
        let mut stmt = conn.prepare("SELECT revoked_at IS NULL FROM members WHERE account_id = ?")?;
        let standing: Option<bool> = stmt
            .query_map(params![account_id], |row| row.get(0))?
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
                "INSERT INTO waitlist (account_id, code) VALUES (?, ?)
                 ON CONFLICT (account_id) DO UPDATE SET
                     code = excluded.code,
                     -- They tried again and were turned away again, so they
                     -- are waiting again — but `seen_at` is untouched, so a
                     -- second attempt does not cost them their place.
                     invited_at = NULL",
                params![account_id, code],
            )?;
            return Ok(Claim::NoRoom);
        }

        conn.execute(
            "INSERT INTO members (account_id, code) VALUES (?, ?)",
            params![account_id, code],
        )?;
        // Nobody who is inside should be chased by a later announce.
        conn.execute("DELETE FROM waitlist WHERE account_id = ?", params![account_id])?;
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
    pub fn revoke(&self, account_id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE members SET revoked_at = current_timestamp
             WHERE account_id = ? AND revoked_at IS NULL",
            params![account_id],
        )?;
        Ok(changed > 0)
    }

    /// Undoes a revoke. False when they were not revoked. Consumes no seat,
    /// for the same reason revoking returned none.
    pub fn restore(&self, account_id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE members SET revoked_at = NULL WHERE account_id = ? AND revoked_at IS NOT NULL",
            params![account_id],
        )?;
        Ok(changed > 0)
    }

    /// Who an announce should reach, as (user, chat), oldest first — so if
    /// the new round is smaller than the queue, the people who have waited
    /// longest hear first.
    pub fn waitlist_to_invite(&self) -> Result<Vec<(i64, i64)>> {
        let conn = self.conn.lock().unwrap();
        // An inner join, deliberately: someone with no recorded address
        // cannot be reached, and silently announcing to nobody would look
        // like a delivered invitation.
        let mut stmt = conn.prepare(
            "SELECT w.account_id, CAST(d.address AS BIGINT)
             FROM waitlist w
             JOIN deliveries d ON d.account_id = w.account_id AND d.channel = 'telegram'
             WHERE w.invited_at IS NULL
             ORDER BY w.seen_at ASC, w.account_id ASC",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Stamped per successful send, so re-running an announce reaches only
    /// the people the first run missed.
    pub fn mark_invited(&self, account_id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE waitlist SET invited_at = current_timestamp WHERE account_id = ?",
            params![account_id],
        )?;
        Ok(())
    }

    /// Drops somebody from the queue. Used when a send proves they cannot
    /// be reached at all — they blocked the bot or deleted the chat, which
    /// is an opt-out, and carrying them forward would mean retrying that
    /// same failure at every future round.
    pub fn forget_waitlist(&self, account_id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM waitlist WHERE account_id = ?", params![account_id])?;
        Ok(())
    }

    /// Requests this user has made since midnight, for the daily cap.
    ///
    /// Reactions and flight searches are excluded: a reaction is not a
    /// request, and a flight search is a sub-event of a message already
    /// counted, so counting it would charge one message twice.
    ///
    /// Midnight is UTC because that is how the rows are stored, not because
    /// UTC is nicer. `created_at` is a naive TIMESTAMP defaulting to
    /// `current_timestamp`, and DuckDB writes that as the UTC instant with
    /// the zone stripped, while `current_date` stays local. Measured on a
    /// CEST machine: stored `2026-08-29 22:35`, `current_date`
    /// `2026-08-30`. This used to compare against `current_date`, which
    /// therefore excluded every row written since local midnight and
    /// counted zero — the cap switching itself off for as many hours as the
    /// offset. The container runs UTC, so the two agreed and production
    /// never saw it. The test suite did, nightly.
    pub fn requests_today(&self, account_id: i64) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT count(*) FROM request_log
             WHERE account_id = ? AND kind IN ('text', 'photo')
               AND created_at >= CAST(current_timestamp AT TIME ZONE 'UTC' AS DATE)",
        )?;
        let n: i64 = stmt
            .query_map(params![account_id], |row| row.get(0))?
            .next()
            .transpose()?
            .unwrap_or(0);
        Ok(n)
    }

    /// Flight searches per user since `cutoff`, scoped to one user. See
    /// [`FLIGHT_SEARCH`].
    pub fn flight_searches_for(&self, cutoff: &str, account_id: i64) -> Result<BTreeMap<i64, i64>> {
        self.kind_counts(cutoff, Self::FLIGHT_SEARCH, Some(account_id))
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
        account_id: Option<i64>,
    ) -> Result<BTreeMap<i64, i64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT account_id, count(*) FROM request_log
             WHERE kind = ? AND created_at >= CAST(? AS TIMESTAMP)
               AND (? IS NULL OR account_id = ?)
             GROUP BY account_id ORDER BY account_id ASC",
        )?;
        let rows = stmt.query_map(params![kind, cutoff, account_id, account_id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Last-seen display name per user id. Small enough to read whole —
    /// one row per person who has ever messaged the bot.
    /// Accounts with a Telegram identity but no recorded display name, as
    /// (account, telegram id).
    ///
    /// Both ids, because the caller needs the Telegram one to ask Telegram
    /// and the account one to file the answer. Handing back only the account
    /// id is what let a display-name backfill call `get_chat` on an account
    /// id and mint a bogus identity from the result.
    pub fn accounts_missing_display_names(&self, limit: usize) -> Result<Vec<(i64, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT i.account_id, CAST(i.external_id AS BIGINT)
             FROM identities i
             WHERE i.kind = 'telegram'
               AND NOT EXISTS (SELECT 1 FROM users u WHERE u.account_id = i.account_id)
             ORDER BY i.account_id ASC LIMIT ?",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    pub fn display_names(&self) -> Result<BTreeMap<i64, String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT account_id, display_name FROM users")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Creates the trip if the name is new, otherwise updates only what was
    /// supplied. Two statements rather than one `ON CONFLICT DO UPDATE`: with
    /// upsert, an unsupplied `adults` would arrive as the insert's default and
    /// overwrite a value already set.
    pub fn upsert_trip(
        &self,
        account_id: i64,
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
            "INSERT INTO trips (account_id, name, name_key) VALUES (?, ?, ?)
             ON CONFLICT (account_id, name_key) DO NOTHING",
            params![account_id, name, key],
        )?;
        // A freshly created trip already starts in `planning`, and a call
        // that supplies neither field is how find-or-create works — it must
        // stay inert. Only an edit to an *existing* trip's price-relevant
        // fields can invalidate prices it was finalised at.
        let invalidates_prices = inserted == 0 && (adults.is_some() || cabin_class.is_some());
        if let Some(adults) = adults {
            conn.execute(
                "UPDATE trips SET adults = ?, updated_at = current_timestamp
                 WHERE account_id = ? AND name_key = ?",
                params![adults, account_id, key],
            )?;
        }
        if let Some(cabin) = cabin_class {
            conn.execute(
                "UPDATE trips SET cabin_class = ?, updated_at = current_timestamp
                 WHERE account_id = ? AND name_key = ?",
                params![cabin, account_id, key],
            )?;
        }
        if invalidates_prices {
            conn.execute(
                "UPDATE trips SET status = 'planning', updated_at = current_timestamp
                 WHERE account_id = ? AND name_key = ?",
                params![account_id, key],
            )?;
        }
        let id: i64 = conn.query_row(
            "SELECT id FROM trips WHERE account_id = ? AND name_key = ?",
            params![account_id, key],
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

    pub fn find_trip(&self, account_id: i64, name: &str) -> Result<Option<Trip>> {
        let key = name.trim().to_lowercase();
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id FROM trips WHERE account_id = ? AND name_key = ?")?;
        let id: Option<i64> = stmt
            .query_map(params![account_id, key], |row| row.get(0))?
            .next()
            .transpose()?;
        match id {
            Some(id) => Ok(Some(load_trip(&conn, id)?)),
            None => Ok(None),
        }
    }

    /// Every trip this user has, newest activity first.
    pub fn list_trips(&self, account_id: i64) -> Result<Vec<Trip>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id FROM trips WHERE account_id = ? ORDER BY updated_at DESC, id DESC")?;
        let ids: Vec<i64> = stmt
            .query_map(params![account_id], |row| row.get(0))?
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
    pub fn delete_trip(&self, account_id: i64, name: &str) -> Result<bool> {
        let key = name.trim().to_lowercase();
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id FROM trips WHERE account_id = ? AND name_key = ?")?;
        let mut ids = stmt.query_map(params![account_id, key], |row| row.get::<_, i64>(0))?;
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
        account_id: row.get(1)?,
        channel: row.get(2)?,
        address: row.get(3)?,
        item: row.get(4)?,
        interval_days: row.get(5)?,
        next_due: row.get(6)?,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tempfile::TempDir;

    pub(crate) fn test_store() -> (Store, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("test.duckdb")).unwrap();
        (store, dir)
    }

    /// The schema exactly as it stood before phase one, frozen. Do not
    /// update this when `MIGRATIONS` changes — its whole value is being an
    /// honest picture of the database the migration will actually meet.
    const LEGACY_SCHEMA: &str = r#"
CREATE SEQUENCE purchases_id_seq;
CREATE TABLE purchases (
    id BIGINT PRIMARY KEY DEFAULT nextval('purchases_id_seq'),
    user_id BIGINT NOT NULL, item TEXT NOT NULL, store TEXT NOT NULL,
    url TEXT, price DOUBLE, currency TEXT, notes TEXT, purchased_at TEXT,
    recorded_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE SEQUENCE reminders_id_seq;
CREATE TABLE reminders (
    id BIGINT PRIMARY KEY DEFAULT nextval('reminders_id_seq'),
    user_id BIGINT NOT NULL, chat_id BIGINT NOT NULL, item TEXT NOT NULL,
    interval_days BIGINT NOT NULL, next_due TEXT NOT NULL,
    active BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE TABLE user_facts (
    user_id BIGINT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (user_id, key)
);
CREATE TABLE request_log (
    user_id BIGINT NOT NULL, kind TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE TABLE users (
    user_id BIGINT PRIMARY KEY, display_name TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE TABLE user_chats (
    user_id BIGINT PRIMARY KEY, chat_id BIGINT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE TABLE invite_rounds (
    code TEXT PRIMARY KEY, capacity BIGINT NOT NULL,
    open BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE TABLE members (
    user_id BIGINT PRIMARY KEY, code TEXT NOT NULL,
    joined_at TIMESTAMP NOT NULL DEFAULT current_timestamp, revoked_at TIMESTAMP
);
CREATE TABLE waitlist (
    user_id BIGINT PRIMARY KEY, chat_id BIGINT NOT NULL, code TEXT NOT NULL,
    seen_at TIMESTAMP NOT NULL DEFAULT current_timestamp, invited_at TIMESTAMP
);
CREATE SEQUENCE trips_id_seq;
CREATE TABLE trips (
    id BIGINT PRIMARY KEY DEFAULT nextval('trips_id_seq'),
    user_id BIGINT NOT NULL, name TEXT NOT NULL, name_key TEXT NOT NULL,
    adults BIGINT NOT NULL DEFAULT 1, cabin_class TEXT,
    status TEXT NOT NULL DEFAULT 'planning',
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    UNIQUE (user_id, name_key)
);
CREATE TABLE trip_segments (
    trip_id BIGINT NOT NULL, position BIGINT NOT NULL, origin TEXT NOT NULL,
    destination TEXT NOT NULL, departure_date TEXT NOT NULL,
    next_candidate BIGINT NOT NULL DEFAULT 1, PRIMARY KEY (trip_id, position)
);
CREATE TABLE segment_candidates (
    trip_id BIGINT NOT NULL, position BIGINT NOT NULL, candidate BIGINT NOT NULL,
    chosen BOOLEAN NOT NULL DEFAULT false, airline TEXT NOT NULL,
    flight_numbers TEXT NOT NULL, itinerary TEXT NOT NULL,
    departing_at_local TEXT, arriving_at_local TEXT, duration_minutes BIGINT,
    quoted_price DOUBLE, quoted_currency TEXT, quoted_at TIMESTAMP, source TEXT,
    PRIMARY KEY (trip_id, position, candidate)
);
"#;

    /// Two Telegram users with data spread across every table, written into
    /// a database that has never seen a migration step. User 33 appears only
    /// on the waitlist, which is what catches a backfill that reads accounts
    /// from the wrong set of tables.
    pub(crate) fn legacy_db() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("legacy.duckdb");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(LEGACY_SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO purchases (user_id, item, store) VALUES (11,'beans','Amazon'),(22,'mouse','eBay');
             INSERT INTO reminders (user_id, chat_id, item, interval_days, next_due)
                 VALUES (11, 555, 'beans', 30, '2026-09-01');
             INSERT INTO user_facts VALUES (11,'currency','EUR',current_timestamp);
             INSERT INTO request_log (user_id, kind) VALUES (11,'text'),(11,'photo'),(22,'text');
             INSERT INTO users (user_id, display_name) VALUES (11,'Ann'),(22,'Bo');
             INSERT INTO user_chats (user_id, chat_id) VALUES (11,555),(22,666);
             INSERT INTO invite_rounds (code, capacity) VALUES ('spring', 5);
             INSERT INTO members (user_id, code) VALUES (22,'spring');
             INSERT INTO waitlist (user_id, chat_id, code) VALUES (33,777,'spring');
             INSERT INTO trips (user_id, name, name_key) VALUES (11,'Lisbon','lisbon');",
        )
        .unwrap();
        drop(conn);
        (dir, path)
    }

    #[test]
    fn missing_names_come_back_with_both_ids_because_they_are_different_numbers() {
        let (s, _d) = test_store();
        let a = s.account_for_telegram(8849043058).unwrap();
        let b = s.account_for_telegram(1980797790).unwrap();
        s.remember_user(a, "Ann").unwrap();

        // Only b is missing, and it comes back as (account, telegram) — two
        // very different numbers. An account id is a small counter; a
        // Telegram id is ten digits. Using one where the other belongs
        // matches nothing, silently.
        let missing = s.accounts_missing_display_names(10).unwrap();
        assert_eq!(missing, vec![(b, 1980797790)]);
        assert_ne!(b, 1980797790, "the two id spaces must not be confused");

        s.remember_user(b, "Bo").unwrap();
        assert!(s.accounts_missing_display_names(10).unwrap().is_empty());
    }

    #[test]
    fn an_identity_is_looked_up_or_created_whatever_its_kind() {
        let (s, _d) = test_store();
        let first = s.account_for_identity("email", "a@example.com").unwrap();
        let again = s.account_for_identity("email", "a@example.com").unwrap();
        assert_eq!(first, again, "the same identity produced two accounts");

        // The generalisation must not have changed what Telegram does.
        let tg = s.account_for_identity("telegram", "11").unwrap();
        assert_eq!(tg, s.account_for_telegram(11).unwrap());
        assert_ne!(tg, first, "two kinds collided into one account");
    }

    #[test]
    fn an_identity_owned_by_someone_else_is_never_moved() {
        let (s, _d) = test_store();
        let owner = s.account_for_identity("telegram", "11").unwrap();
        let other = s.account_for_identity("email", "b@example.com").unwrap();

        assert_eq!(
            s.link_identity(other, "telegram", "11").unwrap(),
            LinkOutcome::TakenByAnother
        );
        // The point of the test: the refusal left ownership alone.
        assert_eq!(s.account_for_identity("telegram", "11").unwrap(), owner);

        assert_eq!(
            s.link_identity(owner, "telegram", "11").unwrap(),
            LinkOutcome::AlreadyYours
        );
        assert_eq!(
            s.link_identity(owner, "email", "c@example.com").unwrap(),
            LinkOutcome::Linked
        );
        assert_eq!(s.account_for_identity("email", "c@example.com").unwrap(), owner);
    }

    #[test]
    fn a_fresh_database_has_somewhere_to_put_login_tokens() {
        let (s, _d) = test_store();
        assert_eq!(s.schema_version().unwrap(), 6);
        // A fresh database is built by MIGRATIONS and a migrated one by
        // steps(); this fails if only one of the two learned about the table.
        s.issue_login_token("hash-x", "a@example.com", None, 900).unwrap();
    }

    #[test]
    fn a_migrated_database_gets_login_tokens_too_not_just_a_fresh_one() {
        // `test_store` opens a fresh file; production opens one at schema
        // 5. This covers the second, which nothing else did.
        //
        // What it does NOT cover, established by breaking each in turn:
        // STEP_6 does not create this table. `Store::open` runs MIGRATIONS
        // unconditionally before applying any step, and MIGRATIONS is all
        // CREATE TABLE IF NOT EXISTS, so the table appears on every
        // database either way — renaming the table inside STEP_6 leaves
        // both tests green. What STEP_6 earns is the version bump, which
        // these tests do pin: dropping it from steps() fails both on
        // `schema_version`. That bump is not bookkeeping either. A pending
        // step is what makes the migration runner take a backup first, so
        // it is the reason this deploy copies the database before touching
        // it.
        let (_dir, path) = legacy_db();
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), 6);
        store.issue_login_token("hash-migrated", "m@example.com", None, 900).unwrap();
        assert_eq!(
            store.consume_login_token("hash-migrated").unwrap(),
            TokenOutcome::Valid { email: "m@example.com".to_string(), account_id: None }
        );
    }

    #[test]
    fn a_token_works_once_and_says_so_afterwards() {
        let (s, _d) = test_store();
        s.issue_login_token("hash-a", "a@example.com", None, 900).unwrap();

        assert_eq!(
            s.consume_login_token("hash-a").unwrap(),
            TokenOutcome::Valid { email: "a@example.com".to_string(), account_id: None }
        );
        // The whole reason the row survives consumption: the second visit
        // gets advice ("you may already be signed in"), not "expired".
        assert_eq!(s.consume_login_token("hash-a").unwrap(), TokenOutcome::AlreadyUsed);
        assert_eq!(s.consume_login_token("hash-never").unwrap(), TokenOutcome::Unknown);
    }

    #[test]
    fn an_expired_token_is_refused_and_not_consumed() {
        let (s, _d) = test_store();
        s.issue_login_token("hash-b", "b@example.com", None, -1).unwrap();
        assert_eq!(s.consume_login_token("hash-b").unwrap(), TokenOutcome::Expired);
        // Expiry must not silently mark it used, or the advice above flips
        // to the wrong branch for anyone who clicks twice.
        assert_eq!(s.consume_login_token("hash-b").unwrap(), TokenOutcome::Expired);
    }

    #[test]
    fn a_link_issued_while_signed_in_remembers_whose_it_is() {
        let (s, _d) = test_store();
        let account = s.account_for_identity("telegram", "11").unwrap();
        s.issue_login_token("hash-c", "c@example.com", Some(account), 900).unwrap();
        assert_eq!(
            s.consume_login_token("hash-c").unwrap(),
            TokenOutcome::Valid { email: "c@example.com".to_string(), account_id: Some(account) }
        );
    }

    #[test]
    fn usage_is_counted_by_account_not_by_telegram_id() {
        let (s, _d) = test_store();
        let account = s.account_for_telegram(8849043058).unwrap();
        s.log_request(account, "text").unwrap();

        let cutoff = "1970-01-01 00:00:00";
        assert_eq!(s.usage_stats_for(cutoff, account).unwrap().len(), 1);
        // The trap: a Telegram id here is a valid i64 and matches nothing,
        // so /stat quietly reported an empty week instead of failing.
        assert!(s.usage_stats_for(cutoff, 8849043058).unwrap().is_empty());
    }

    #[test]
    fn a_conversation_round_trips_and_is_scoped() {
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();

        let direct = s.start_conversation(a, "direct").unwrap();
        let group = s.start_conversation(a, "telegram:-100").unwrap();
        assert_ne!(direct, group, "a group must not share the private thread");

        s.replace_messages(
            direct,
            &[
                r#"{"role":"user","content":"hi"}"#.to_string(),
                r#"{"role":"assistant","content":"hello"}"#.to_string(),
            ],
        )
        .unwrap();

        let bodies = s.conversation_messages(direct, 20).unwrap();
        assert_eq!(bodies.len(), 2);
        assert!(bodies[0].contains("hi"), "oldest first");

        assert!(s.conversation_messages(group, 20).unwrap().is_empty());
        // Each scope reports its own newest thread, never the other's.
        assert_eq!(s.latest_conversation(a, "direct", 0).unwrap().unwrap().0, direct);
        assert_eq!(s.latest_conversation(a, "telegram:-100", 0).unwrap().unwrap().0, group);
    }

    #[test]
    fn a_conversation_returns_only_the_last_n_messages_oldest_first() {
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let c = s.start_conversation(a, "direct").unwrap();
        let bodies: Vec<String> = (0..25).map(|i| format!(r#"{{"n":{i}}}"#)).collect();
        s.replace_messages(c, &bodies).unwrap();

        let got = s.conversation_messages(c, 20).unwrap();
        assert_eq!(got.len(), 20);
        assert!(got[0].contains(r#""n":5"#), "should drop the oldest five");
        assert!(got[19].contains(r#""n":24"#), "and end at the newest");
    }

    #[test]
    fn a_quiet_conversation_ages_out_and_a_live_one_does_not() {
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let first = s.start_conversation(a, "direct").unwrap();

        // Inside the TTL: reported as live, so the caller keeps using it.
        assert_eq!(s.latest_conversation(a, "direct", 600).unwrap(), Some((first, false)));

        // Push it back beyond the TTL and the same call reports it stale.
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE conversations
                 SET updated_at = CAST(current_timestamp AS TIMESTAMP) - to_seconds(3600)
                 WHERE id = ?",
                params![first],
            )
            .unwrap();
        }
        assert_eq!(s.latest_conversation(a, "direct", 600).unwrap(), Some((first, true)));

        // Speaking in it makes it live again without starting a new one.
        s.touch_conversation(first).unwrap();
        assert_eq!(s.latest_conversation(a, "direct", 600).unwrap(), Some((first, false)));

        // An account with nothing has nothing — the caller starts the thread.
        let b = s.account_for_telegram(22).unwrap();
        assert_eq!(s.latest_conversation(b, "direct", 600).unwrap(), None);
    }

    #[test]
    fn where_to_reach_someone_moves_into_deliveries() {
        let (_d, path) = legacy_db();
        let s = Store::open(&path).unwrap();
        let conn = s.conn.lock().unwrap();

        let account_of = |tg: &str| -> i64 {
            conn.query_row(
                "SELECT account_id FROM identities WHERE kind='telegram' AND external_id = ?",
                params![tg], |r| r.get(0),
            ).unwrap()
        };

        let addr: String = conn
            .query_row("SELECT address FROM deliveries WHERE account_id = ? AND channel='telegram'",
                params![account_of("11")], |r| r.get(0)).unwrap();
        assert_eq!(addr, "555");

        // The waitlisted person had a chat_id but no user_chats row; the
        // waitlist was the only record of where to reach them, and losing it
        // would silently break the next announce.
        let addr: String = conn
            .query_row("SELECT address FROM deliveries WHERE account_id = ? AND channel='telegram'",
                params![account_of("33")], |r| r.get(0)).unwrap();
        assert_eq!(addr, "777");

        let gone: i64 = conn
            .query_row("SELECT count(*) FROM information_schema.tables WHERE table_name='user_chats'",
                [], |r| r.get(0)).unwrap();
        assert_eq!(gone, 0, "user_chats should be gone");
    }

    #[test]
    fn constrained_tables_are_rebuilt_around_accounts() {
        let (_d, path) = legacy_db();
        let s = Store::open(&path).unwrap();
        let conn = s.conn.lock().unwrap();

        let account_of = |tg: &str| -> i64 {
            conn.query_row(
                "SELECT account_id FROM identities WHERE kind='telegram' AND external_id = ?",
                params![tg], |r| r.get(0),
            ).unwrap()
        };
        let (ann, bo, cy) = (account_of("11"), account_of("22"), account_of("33"));

        let fact: String = conn
            .query_row("SELECT value FROM user_facts WHERE account_id = ? AND key='currency'",
                params![ann], |r| r.get(0)).unwrap();
        assert_eq!(fact, "EUR");

        let name: String = conn
            .query_row("SELECT display_name FROM users WHERE account_id = ?", params![bo], |r| r.get(0)).unwrap();
        assert_eq!(name, "Bo");

        let code: String = conn
            .query_row("SELECT code FROM members WHERE account_id = ?", params![bo], |r| r.get(0)).unwrap();
        assert_eq!(code, "spring");

        let waiting: i64 = conn
            .query_row("SELECT count(*) FROM waitlist WHERE account_id = ?", params![cy], |r| r.get(0)).unwrap();
        assert_eq!(waiting, 1);

        let trip: String = conn
            .query_row("SELECT name FROM trips WHERE account_id = ?", params![ann], |r| r.get(0)).unwrap();
        assert_eq!(trip, "Lisbon");

        // The waitlisted person's chat id had to be rescued into deliveries
        // before the rebuild dropped the column.
        let addr: String = conn
            .query_row("SELECT address FROM deliveries WHERE account_id = ? AND channel='telegram'",
                params![cy], |r| r.get(0)).unwrap();
        assert_eq!(addr, "777");

        // The trips sequence still hands out fresh ids after the rebuild.
        conn.execute("INSERT INTO trips (account_id, name, name_key) VALUES (?, 'Porto', 'porto')",
            params![ann]).unwrap();
        let ids: i64 = conn.query_row("SELECT count(DISTINCT id) FROM trips", [], |r| r.get(0)).unwrap();
        assert_eq!(ids, 2, "a rebuilt table must not reuse an id");
    }

    #[test]
    fn unconstrained_tables_carry_account_ids_and_keep_their_rows() {
        let (_d, path) = legacy_db();
        let s = Store::open(&path).unwrap();
        let conn = s.conn.lock().unwrap();

        let ann: i64 = conn
            .query_row(
                "SELECT account_id FROM identities WHERE kind='telegram' AND external_id='11'",
                [], |r| r.get(0),
            )
            .unwrap();

        let purchases: i64 = conn
            .query_row("SELECT count(*) FROM purchases WHERE account_id = ?", params![ann], |r| r.get(0))
            .unwrap();
        assert_eq!(purchases, 1, "Ann's beans should have moved across");

        let logged: i64 = conn
            .query_row("SELECT count(*) FROM request_log WHERE account_id = ?", params![ann], |r| r.get(0))
            .unwrap();
        assert_eq!(logged, 2);

        // A reminder is delivered where it was created, so it carries its
        // own address rather than inheriting the account default.
        let addr: String = conn
            .query_row("SELECT address FROM reminders WHERE item = 'beans'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(addr, "555");

        // Ids and their sequences survive the rebuild.
        conn.execute("INSERT INTO purchases (account_id, item, store) VALUES (?, 'cable', 'eBay')",
            params![ann]).unwrap();
        let distinct: i64 = conn
            .query_row("SELECT count(DISTINCT id) FROM purchases", [], |r| r.get(0)).unwrap();
        let total: i64 = conn.query_row("SELECT count(*) FROM purchases", [], |r| r.get(0)).unwrap();
        assert_eq!(distinct, total, "a rebuilt table must not reuse an id");

        // Nothing orphaned anywhere.
        for table in ["purchases", "reminders", "request_log"] {
            let orphans: i64 = conn
                .query_row(&format!("SELECT count(*) FROM {table} WHERE account_id IS NULL"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(orphans, 0, "{table} has rows with no account");
        }
    }

    #[test]
    fn every_legacy_user_gets_exactly_one_account() {
        let (_d, path) = legacy_db();
        let s = Store::open(&path).unwrap();
        let conn = s.conn.lock().unwrap();

        // 11, 22 and 33 — 33 exists only on the waitlist.
        let accounts: i64 = conn.query_row("SELECT count(*) FROM accounts", [], |r| r.get(0)).unwrap();
        assert_eq!(accounts, 3);

        let ids: i64 = conn
            .query_row(
                "SELECT count(*) FROM identities WHERE kind = 'telegram' \
                 AND external_id IN ('11','22','33')",
                [], |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ids, 3);

        // No Telegram id claimed twice.
        let dupes: i64 = conn
            .query_row(
                "SELECT count(*) FROM (SELECT external_id FROM identities \
                 WHERE kind='telegram' GROUP BY external_id HAVING count(*) > 1)",
                [], |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dupes, 0);
    }

    #[test]
    fn the_legacy_fixture_has_no_accounts_yet() {
        let (_d, path) = legacy_db();
        let conn = Connection::open(&path).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM information_schema.tables WHERE table_name = 'accounts'",
                [], |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
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
        let r = s.create_reminder(1, "telegram", "10", "coffee", 30, "2026-08-01").unwrap();
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
        let r = s.create_reminder(1, "telegram", "10", "coffee", 30, "2026-08-01").unwrap();
        assert!(!s.cancel_reminder(2, r.id).unwrap());
        assert_eq!(s.list_reminders(1).unwrap().len(), 1);
    }

    #[test]
    fn due_reminders_selects_past_and_today_only() {
        let (s, _d) = test_store();
        s.create_reminder(1, "telegram", "10", "overdue", 30, "2026-07-01").unwrap();
        s.create_reminder(1, "telegram", "10", "today", 30, "2026-07-22").unwrap();
        s.create_reminder(1, "telegram", "10", "future", 30, "2026-09-01").unwrap();
        let cancelled = s.create_reminder(1, "telegram", "10", "cancelled", 30, "2026-07-01").unwrap();
        s.cancel_reminder(1, cancelled.id).unwrap();
        s.create_reminder(2, "telegram", "20", "other-user", 30, "2026-07-02").unwrap();

        let due = s.due_reminders("2026-07-22").unwrap();
        let items: Vec<_> = due.iter().map(|r| r.item.as_str()).collect();
        assert_eq!(items, vec!["overdue", "other-user", "today"]);
    }

    #[test]
    fn set_next_due_updates() {
        let (s, _d) = test_store();
        let r = s.create_reminder(1, "telegram", "10", "coffee", 30, "2026-07-01").unwrap();
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
        s.note_delivery(1, "telegram", "1").unwrap();
        s.note_delivery(2, "telegram", "-100200300").unwrap();
        assert_eq!(s.broadcast_targets().unwrap(), vec![(1, 1), (2, -100200300)]);

        // Moving chats replaces the old one: an announcement should follow
        // the person, not accumulate copies.
        s.note_delivery(1, "telegram", "-999").unwrap();
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
        // Through the resolver, so each member has a telegram identity —
        // which is what `active_members` reports back to the gate.
        for tg in 1..=3 {
            let account = s.account_for_telegram(tg).unwrap();
            assert_eq!(s.claim_seat(account, "autumn").unwrap(), Claim::Admitted);
        }
        let fourth = s.account_for_telegram(4).unwrap();
        assert_eq!(s.claim_seat(fourth, "autumn").unwrap(), Claim::NoRoom);
        assert_eq!(s.active_members().unwrap(), vec![1, 2, 3]);

        let rounds = s.rounds().unwrap();
        assert_eq!(rounds.len(), 1);
        assert_eq!((rounds[0].used, rounds[0].capacity, rounds[0].open), (3, 3, true));
    }

    #[test]
    fn claiming_a_seat_records_no_address_of_its_own() {
        let (s, _d) = test_store();
        s.create_round("autumn", 5).unwrap();
        let account = s.account_for_identity("email", "web@example.com").unwrap();

        assert_eq!(s.claim_seat(account, "autumn").unwrap(), Claim::Admitted);

        // The point of the split: a web visitor has no chat, and claiming a
        // seat must not invent one. If this fails, the delivery write has
        // crept back into claim_seat and the web path writes a lie.
        let conn = s.conn.lock().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM deliveries WHERE account_id = ?",
                params![account],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "claim_seat wrote a delivery row");
    }

    #[test]
    fn concurrent_claims_never_oversell() {
        // The whole reason claiming is one method: check-and-insert happens
        // under a single lock, so a rush on a link cannot overfill a round.
        let (s, _d) = test_store();
        assert!(s.create_round("rush", 5).unwrap());

        let admitted: usize = std::thread::scope(|scope| {
            let handles: Vec<_> = (1..=40)
                .map(|tg| {
                    let s = s.clone();
                    scope.spawn(move || {
                        let account = s.account_for_telegram(tg).unwrap();
                        s.claim_seat(account, "rush").unwrap()
                    })
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
        assert_eq!(s.claim_seat(1, "autumn").unwrap(), Claim::Admitted);
        assert_eq!(s.claim_seat(1, "autumn").unwrap(), Claim::AlreadyIn);
        // A member opening a *later* round's link is also already in.
        s.create_round("winter", 5).unwrap();
        assert_eq!(s.claim_seat(1, "winter").unwrap(), Claim::AlreadyIn);

        assert_eq!(s.rounds().unwrap()[0].used, 1, "one person, one seat");
        assert_eq!(s.claim_seat(2, "autumn").unwrap(), Claim::Admitted);
    }

    #[test]
    fn a_revoked_member_cannot_rejoin_through_any_link() {
        // Without this, revoking is theatre.
        let (s, _d) = test_store();
        s.create_round("autumn", 10).unwrap();
        s.claim_seat(1, "autumn").unwrap();
        assert!(s.revoke(1).unwrap());
        assert!(s.active_members().unwrap().is_empty());

        assert_eq!(s.claim_seat(1, "autumn").unwrap(), Claim::Revoked);
        s.create_round("winter", 10).unwrap();
        assert_eq!(s.claim_seat(1, "winter").unwrap(), Claim::Revoked);

        // And being refused does not put them on the waitlist to be
        // announced back in later.
        assert_eq!(s.waiting_count().unwrap(), 0);
    }

    #[test]
    fn revoking_returns_no_seat_and_restoring_takes_none() {
        let (s, _d) = test_store();
        s.create_round("autumn", 2).unwrap();
        let (a1, a2) = (s.account_for_telegram(1).unwrap(), s.account_for_telegram(2).unwrap());
        s.claim_seat(a1, "autumn").unwrap();
        s.claim_seat(a2, "autumn").unwrap();

        assert!(s.revoke(1).unwrap());
        assert_eq!(s.rounds().unwrap()[0].used, 2, "a round of 2 admitted 2 people, once");
        assert_eq!(s.claim_seat(3, "autumn").unwrap(), Claim::NoRoom);

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

        assert_eq!(s.claim_seat(1, "autumn").unwrap(), Claim::NoRoom);
        assert_eq!(s.claim_seat(2, "no-such-round").unwrap(), Claim::NoRoom);
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
        assert_eq!(s.claim_seat(1, "autumn").unwrap(), Claim::NoRoom);

        assert!(s.set_round_open("autumn", true).unwrap());
        assert_eq!(s.claim_seat(1, "autumn").unwrap(), Claim::Admitted);
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
        s.claim_seat(1, "autumn").unwrap();

        assert_eq!(s.claim_seat(2, "autumn").unwrap(), Claim::NoRoom);
        // claim_seat no longer records where to reach them — that is the
        // channel's job now — so give the waitlist the address it needs.
        s.note_delivery(2, "telegram", "8002").unwrap();
        assert_eq!(s.waitlist_to_invite().unwrap(), vec![(2, 8002)]);

        // A second attempt keeps their place rather than costing it.
        assert_eq!(s.claim_seat(2, "autumn").unwrap(), Claim::NoRoom);
        assert_eq!(s.waiting_count().unwrap(), 1);

        // Getting in through a later round takes them off the queue, so a
        // future announce does not chase somebody already inside.
        s.create_round("winter", 5).unwrap();
        assert_eq!(s.claim_seat(2, "winter").unwrap(), Claim::Admitted);
        assert_eq!(s.waiting_count().unwrap(), 0);
        assert!(s.waitlist_to_invite().unwrap().is_empty());
    }

    #[test]
    fn an_announce_stamps_only_the_rows_it_reached() {
        let (s, _d) = test_store();
        s.create_round("autumn", 0).unwrap();
        for user in [1, 2, 3] {
            s.claim_seat(user, "autumn").unwrap();
            // claim_seat no longer records the address — the channel does.
            s.note_delivery(user, "telegram", &(9000 + user).to_string()).unwrap();
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
        s.claim_seat(1, "autumn").unwrap();
        s.note_delivery(1, "telegram", "9001").unwrap();
        s.mark_invited(1).unwrap();
        assert!(s.waitlist_to_invite().unwrap().is_empty());

        s.create_round("winter", 0).unwrap();
        assert_eq!(s.claim_seat(1, "winter").unwrap(), Claim::NoRoom);
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
    #[test]
    fn a_backup_is_a_whole_database_that_opens_on_its_own() {
        // Taken from the connection that holds the source open, with no
        // checkpoint and nothing stopped. That is the entire point: no
        // outside process can open this file while Scout runs, so a copy
        // made from outside is crash-consistent at best.
        let (store, dir) = test_store();
        let account = store.account_for_telegram(99).unwrap();
        store.remember_user(account, "before the backup").unwrap();

        let backup = dir.path().join("backup.duckdb");
        store.backup_to(&backup).unwrap();

        // The source is undisturbed and still writable.
        store.remember_user(account, "after the backup").unwrap();

        let restored = Store::open(&backup).unwrap();
        assert_eq!(
            restored.display_names().unwrap().get(&account).map(String::as_str),
            Some("before the backup"),
            "the backup should hold what was committed when it was taken"
        );
        assert_eq!(restored.schema_version().unwrap(), store.schema_version().unwrap());
    }

    #[test]
    fn a_pending_migration_backs_the_database_up_before_changing_it() {
        // The failure with no second chance. A migration cannot be undone, so
        // if this copy is missing — or is taken after the fact — a bad schema
        // step is unrecoverable. We took this by hand four times before
        // automating it, and remembering was the only protection.
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("legacy.duckdb");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(LEGACY_SCHEMA).unwrap();
            conn.execute_batch(
                "INSERT INTO purchases (user_id, item, store) VALUES (7, 'detergent', 'bol.com')",
            )
            .unwrap();
        }

        let store = Store::open(&db).unwrap();
        assert!(store.schema_version().unwrap() >= 5, "the migration ran");

        let backups = crate::backup::dir_for(&db);
        let taken: Vec<_> = std::fs::read_dir(&backups)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(taken.len(), 1, "exactly one backup, taken before the steps");
        assert!(taken[0].file_name().unwrap().to_str().unwrap().contains("migration-v"));

        // The proof it was taken BEFORE rather than after: the copy still has
        // the column the migration replaces.
        let before = Connection::open(&taken[0]).unwrap();
        let legacy: i64 = before
            .query_row(
                "SELECT count(*) FROM information_schema.columns
                 WHERE table_name = 'purchases' AND column_name = 'user_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(legacy, 1, "the backup should predate the column being replaced");
    }

    #[test]
    fn a_database_with_nothing_to_migrate_is_not_backed_up() {
        // Otherwise every restart writes one and the retention window stops
        // meaning two weeks.
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("fresh.duckdb");
        drop(Store::open(&db).unwrap());
        drop(Store::open(&db).unwrap());

        let backups = crate::backup::dir_for(&db);
        let n = std::fs::read_dir(&backups).map(|d| d.count()).unwrap_or(0);
        assert_eq!(n, 0, "a fresh database has no pending steps and needs no copy");
    }

}
