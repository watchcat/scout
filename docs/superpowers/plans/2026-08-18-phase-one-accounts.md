# Phase One — Accounts and Persisted Conversations: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the Telegram user id with an internal account id across the
whole schema, and move conversation history out of memory into the database —
so that a later phase can put a web app in front of the same data.

**Architecture:** Scout stays one process. A versioned migration runner is
added beside the existing idempotent `MIGRATIONS` batch, because a data
migration cannot be re-run the way `CREATE TABLE IF NOT EXISTS` can. Every
table that keyed on a Telegram id keys on `accounts.id` instead, and a
Telegram id becomes one row in `identities`. Conversation history becomes
rows in `conversations` and `messages` rather than a `DashMap` on `App`.

**Tech Stack:** Rust, DuckDB v1.5.1 (bundled by the `duckdb` crate), rig-core
0.40 for `Message`, serde_json for message bodies, tempfile for test stores.

---

## Verified before writing this plan

Do not re-litigate these; they were measured, not assumed.

| Question | Answer | How |
|---|---|---|
| DuckDB version in the binary | **v1.5.1** | `SELECT version()` through the crate |
| Python DuckDB on this machine | **1.4.5 — older** | `duckdb.__version__` |
| `ALTER TABLE ADD COLUMN` | works | probe |
| `UPDATE t SET c = x.c FROM x WHERE …` | works | probe |
| `ALTER TABLE DROP COLUMN` | works **only if the column is in no PK/UNIQUE constraint** | probe: `Catalog Error: Cannot drop column "user_id" because it is referenced in unique constraint` |
| `ALTER TABLE RENAME TO` | works, and sequences survive it | probe |
| DDL inside a transaction | transactional — a rolled-back `CREATE TABLE` leaves nothing | probe |
| `rig::completion::Message` | derives `Serialize` + `Deserialize` | `rig-core-0.40.0/src/completion/message.rs:20` |
| `INSERT … RETURNING id` | works | probe |
| `ON CONFLICT (a, b) DO NOTHING` / `DO UPDATE` | works | probe |
| `current_timestamp - to_seconds(n)` | **fails** — `current_timestamp` is TIMESTAMPTZ and there is no `-(TIMESTAMPTZ, INTERVAL)` overload. Must be `CAST(current_timestamp AS TIMESTAMP) - to_seconds(n)` | probe |
| Test suite before starting | 412 passing, 3 ignored | `cargo test` |

The PK/UNIQUE finding splits the nine tables into two groups and is the single
most important fact in this plan:

- **Rewrite in place** (`user_id` in no constraint): `purchases`,
  `reminders`, `request_log`
- **Rebuild the table** (`user_id` is in a PK or UNIQUE): `user_facts`,
  `users`, `members`, `waitlist`, `trips`
- **Replaced entirely**: `user_chats` — it is already
  "where to reach this person", which is what `deliveries` is

## Two decisions this plan makes that the spec left open

**1. Conversations carry a scope.** The spec says one rolling conversation
per account shared across channels. Applied literally that would merge a
group chat's history into the owner's private thread, because today
`chats: DashMap<(chat_id, sender_id), ChatSession>` (`src/bot.rs:28`) keeps
each group separate. So `conversations` has a `scope` column: `'direct'` for a
1:1 Telegram chat and for the web app — those two genuinely share, which is
the point of the decision — and `'telegram:<chat_id>'` for a group. Today's
isolation is preserved; the phone-and-desk sharing still works.

**2. A reminder keeps its own delivery address.** The spec says
`reminders.chat_id` "becomes a row in `deliveries`". Taken literally, a
reminder created in a group would start arriving in the owner's DM, because
`deliveries` holds one default address per channel. So `reminders` gets its
own `channel` + `address`, migrated from `chat_id`, and `deliveries` holds
the account's default address (backfilled from `user_chats`) for announces.
Behaviour is unchanged either way.

## File structure

| File | Responsibility | Change |
|---|---|---|
| `src/store.rs` | schema, migration runner, all queries | modify — new tables, runner, `account_id` params, conversation methods |
| `src/bot.rs` | Telegram adapter | modify — resolve Telegram id → account id; load/save history |
| `src/agent.rs` | `build_agent` | modify — parameter renamed to `account_id` |
| `src/scheduler.rs` | reminder delivery | modify — send to `reminder.address` |
| `src/stats.rs` | `/stat` | modify — joins `identities` for a label |
| `src/tools/*.rs` | tool structs holding `user_id` | modify — field renamed to `account_id` |

`store.rs` is already 2410 lines. This plan adds roughly 400 more, which is
the point at which it should be split — but splitting it is not phase one's
job and would collide with every task here. Leave it; phase two moves it into
`scout-core` and that is the moment to break it up.

---

## Task 1: A versioned migration runner

Today `Store::open` runs one idempotent `CREATE TABLE IF NOT EXISTS` batch
(`src/store.rs:281-287`). Data migrations are not idempotent, so they need a
recorded version.

**Files:**
- Modify: `src/store.rs:281-287` (`Store::open`)
- Test: `src/store.rs` (the existing `mod tests`)

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/store.rs`:

```rust
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
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test migration_steps_run_once_and_only_once`
Expected: FAIL — `no method named 'schema_version' found`

- [ ] **Step 3: Implement the runner**

Replace `Store::open` in `src/store.rs`:

```rust
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
```

Add above `impl Store`:

```rust
/// A numbered change to an existing database. `MIGRATIONS` above creates
/// tables and is safe to re-run; these are not, so each one runs at most
/// once and the number it reached is recorded.
///
/// Never renumber or edit a step that has shipped. Add a new one.
enum Step {
    Sql(&'static str),
    /// For work that needs a loop or a returned id — plain SQL cannot ask
    /// DuckDB for `nextval` per row and keep the mapping.
    Code(fn(&Connection) -> Result<()>),
}

fn steps() -> Vec<(i64, Step)> {
    vec![]
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
        // DDL is transactional in DuckDB v1.5.1 (verified), so a step that
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
```

`steps()` is empty for now, so `schema_version` is 0. The test deliberately
asserts only that the number does not move on reopen — that is the property
the runner must have, and it holds with zero steps or fifty.

- [ ] **Step 4: Run the whole suite**

Run: `cargo test`
Expected: 413 passing, 3 ignored. Everything green.

- [ ] **Step 5: Commit**

```bash
git add src/store.rs
git commit -m "feat: a place to record which migrations have run"
```

---

## Task 2: The new tables

**Files:**
- Modify: `src/store.rs` (`steps()`)
- Test: `src/store.rs` (`mod tests`)

- [ ] **Step 1: Write the failing test**

```rust
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
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test the_new_tables_exist_after_opening`
Expected: FAIL — `assertion failed: 0 == 1, accounts should exist`

- [ ] **Step 3: Add step 1**

Add the const above `steps()` in `src/store.rs`:

```rust
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
```

And return it from `steps()`:

```rust
fn steps() -> Vec<(i64, Step)> {
    vec![(1, Step::Sql(STEP_1_NEW_TABLES))]
}
```

- [ ] **Step 4: Run both tests**

Run: `cargo test the_new_tables_exist_after_opening migration_steps_run_once`
Expected: both PASS — `schema_version` is now 1.

- [ ] **Step 5: Commit**

```bash
git add src/store.rs
git commit -m "feat: accounts, identities, deliveries and conversations exist"
```

---

## Task 3: A pre-migration fixture

Every later task asserts against a database shaped the way production is
shaped *today*. That fixture has to be frozen in the test file, because
`MIGRATIONS` will keep changing.

**Files:**
- Modify: `src/store.rs` (`mod tests`)

- [ ] **Step 1: Add the frozen legacy schema and a builder**

Add to `mod tests`:

```rust
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
    /// a database that has never seen a migration step. Returns the path.
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
```

Note that user 33 appears **only** in `waitlist` — that is deliberate. It
catches a backfill that reads accounts from the wrong set of tables.

- [ ] **Step 2: Write a test proving the fixture is pre-migration**

```rust
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
```

- [ ] **Step 3: Run it**

Run: `cargo test the_legacy_fixture_has_no_accounts_yet`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add src/store.rs
git commit -m "test: a frozen picture of the database the migration will meet"
```

---

## Task 4: Mint an account for every existing user

**Files:**
- Modify: `src/store.rs` (`steps()`, new `step_2_backfill_accounts`)
- Test: `src/store.rs` (`mod tests`)

- [ ] **Step 1: Write the failing test**

```rust
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
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test every_legacy_user_gets_exactly_one_account`
Expected: FAIL — `assertion failed: 0 == 3`

- [ ] **Step 3: Implement the backfill**

Add to `src/store.rs`, above `steps()`:

```rust
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
    let ids: Vec<i64> = stmt.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
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
```

Extend `steps()`:

```rust
fn steps() -> Vec<(i64, Step)> {
    vec![
        (1, Step::Sql(STEP_1_NEW_TABLES)),
        (2, Step::Code(step_2_backfill_accounts)),
    ]
}
```

- [ ] **Step 4: Run it**

Run: `cargo test every_legacy_user_gets_exactly_one_account`
Expected: PASS

- [ ] **Step 5: Prove the guard by breaking it**

Temporarily drop `UNION SELECT user_id FROM waitlist` from
`LEGACY_USER_IDS`, run the test, and confirm it fails with `2 == 3`. Restore
it. This is the check that user 33 exists for.

- [ ] **Step 6: Commit**

```bash
git add src/store.rs
git commit -m "feat: every existing user becomes an account with a telegram identity"
```

---

## Task 5: Rewrite the three unconstrained tables

`purchases`, `reminders` and `request_log` hold `user_id` in no PK or UNIQUE
constraint, so they can be altered in place.

**Files:**
- Modify: `src/store.rs` (`steps()`)
- Test: `src/store.rs` (`mod tests`)

- [ ] **Step 1: Write the failing test**

```rust
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

        // Nothing orphaned anywhere.
        for table in ["purchases", "reminders", "request_log"] {
            let orphans: i64 = conn
                .query_row(&format!("SELECT count(*) FROM {table} WHERE account_id IS NULL"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(orphans, 0, "{table} has rows with no account");
        }
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test unconstrained_tables_carry_account_ids`
Expected: FAIL — `Binder Error: Referenced column "account_id" not found`

- [ ] **Step 3: Add step 3**

```rust
const STEP_3_UNCONSTRAINED: &str = r#"
ALTER TABLE purchases ADD COLUMN account_id BIGINT;
UPDATE purchases SET account_id = i.account_id FROM identities i
    WHERE i.kind = 'telegram' AND i.external_id = CAST(purchases.user_id AS TEXT);
ALTER TABLE purchases DROP COLUMN user_id;

ALTER TABLE reminders ADD COLUMN account_id BIGINT;
UPDATE reminders SET account_id = i.account_id FROM identities i
    WHERE i.kind = 'telegram' AND i.external_id = CAST(reminders.user_id AS TEXT);
ALTER TABLE reminders DROP COLUMN user_id;

ALTER TABLE request_log ADD COLUMN account_id BIGINT;
UPDATE request_log SET account_id = i.account_id FROM identities i
    WHERE i.kind = 'telegram' AND i.external_id = CAST(request_log.user_id AS TEXT);
ALTER TABLE request_log DROP COLUMN user_id;
"#;
```

Add `(3, Step::Sql(STEP_3_UNCONSTRAINED))` to `steps()`.

- [ ] **Step 4: Run it**

Run: `cargo test unconstrained_tables_carry_account_ids`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/store.rs
git commit -m "feat: purchases, reminders and the request log key on accounts"
```

---

## Task 6: Rebuild the five constrained tables

`user_facts`, `users`, `members`, `waitlist` and `trips` all hold `user_id`
inside a PK or UNIQUE constraint, and DuckDB refuses to drop such a column
(verified: `Cannot drop column "user_id" because it is referenced in unique
constraint`). Each is rebuilt: create the new shape, copy through the join,
drop, rename.

**Files:**
- Modify: `src/store.rs` (`steps()`)
- Test: `src/store.rs` (`mod tests`)

- [ ] **Step 1: Write the failing test**

```rust
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
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test constrained_tables_are_rebuilt_around_accounts`
Expected: FAIL — `Binder Error: Referenced column "account_id" not found`

- [ ] **Step 3: Add step 4**

```rust
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
INSERT INTO trips_new (id, account_id, name, name_key, adults, cabin_class, status, created_at, updated_at)
SELECT t.id, i.account_id, t.name, t.name_key, t.adults, t.cabin_class, t.status, t.created_at, t.updated_at
FROM trips t
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(t.user_id AS TEXT);
DROP TABLE trips;
ALTER TABLE trips_new RENAME TO trips;
"#;
```

Add `(4, Step::Sql(STEP_4_REBUILDS))` to `steps()`.

Note what the rebuilt `waitlist` does *not* have: `chat_id`. It is captured
into `deliveries` by the first statement of this same step, which is why
that statement must stay at the top. `user_chats` is untouched here and is
retired in Task 7.

- [ ] **Step 4: Run it**

Run: `cargo test constrained_tables_are_rebuilt_around_accounts`
Expected: PASS

- [ ] **Step 5: Run everything**

Run: `cargo test`
Expected: some existing store tests now FAIL to compile or assert, because
`Store` methods still say `user_id` and query dropped columns. That is
expected and Task 8 fixes it. Do not commit a broken build — go straight on
if the crate no longer compiles.

- [ ] **Step 6: Commit**

```bash
git add src/store.rs
git commit -m "feat: rebuild the constrained tables around account ids"
```

---

## Task 7: Deliveries replace user_chats

**Files:**
- Modify: `src/store.rs` (`steps()`)
- Test: `src/store.rs` (`mod tests`)

- [ ] **Step 1: Write the failing test**

```rust
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
        // waitlist is the only record of where to reach them, and losing it
        // would silently break the next announce.
        let addr: String = conn
            .query_row("SELECT address FROM deliveries WHERE account_id = ? AND channel='telegram'",
                params![account_of("33")], |r| r.get(0)).unwrap();
        assert_eq!(addr, "777");

        // A reminder keeps its own address rather than inheriting the default.
        let addr: String = conn
            .query_row("SELECT address FROM reminders WHERE item = 'beans'", [], |r| r.get(0)).unwrap();
        assert_eq!(addr, "555");

        let gone: i64 = conn
            .query_row("SELECT count(*) FROM information_schema.tables WHERE table_name='user_chats'",
                [], |r| r.get(0)).unwrap();
        assert_eq!(gone, 0, "user_chats should be gone");
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test where_to_reach_someone_moves_into_deliveries`
Expected: FAIL — `deliveries` is empty, so `query_row` returns
`QueryReturnedNoRows`.

- [ ] **Step 3: Add step 5**

The waitlist rescue already happened at the top of step 4, before the column
was dropped. This step handles `user_chats` — which is still intact — and
the reminders' own addresses.

```rust
const STEP_5_DELIVERIES: &str = r#"
-- Default address per channel, from wherever each person last spoke. This
-- overwrites the waitlist address rescued in step 4 where both exist, which
-- is right: user_chats is the more recent record of where someone is.
INSERT INTO deliveries (account_id, channel, address)
SELECT i.account_id, 'telegram', CAST(c.chat_id AS TEXT)
FROM user_chats c
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(c.user_id AS TEXT)
ON CONFLICT (account_id, channel)
DO UPDATE SET address = excluded.address, updated_at = current_timestamp;

-- A reminder is delivered where it was created, not where the account was
-- last seen, so it carries its own address.
ALTER TABLE reminders ADD COLUMN channel TEXT NOT NULL DEFAULT 'telegram';
ALTER TABLE reminders ADD COLUMN address TEXT;
UPDATE reminders SET address = CAST(chat_id AS TEXT);
ALTER TABLE reminders DROP COLUMN chat_id;

DROP TABLE user_chats;
"#;
```

Set `steps()` to its final form:

```rust
fn steps() -> Vec<(i64, Step)> {
    vec![
        (1, Step::Sql(STEP_1_NEW_TABLES)),
        (2, Step::Code(step_2_backfill_accounts)),
        (3, Step::Sql(STEP_3_UNCONSTRAINED)),
        (4, Step::Sql(STEP_4_REBUILDS)),
        (5, Step::Sql(STEP_5_DELIVERIES)),
    ]
}
```

**Never insert a step with a number below one that has already run.** The
runner skips anything at or below the recorded version, so a step slipped in
underneath would be silently ignored on every database that has passed it —
including production. Always append.

- [ ] **Step 4: Run it**

Run: `cargo test where_to_reach_someone_moves_into_deliveries`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/store.rs
git commit -m "feat: deliveries hold where to reach an account"
```

---

## Task 8: Store speaks account ids

Every `Store` method takes `user_id: i64` and queries a column that no longer
exists. This is mechanical but large: `src/store.rs` has 124 occurrences.

**Files:**
- Modify: `src/store.rs` — all query methods
- Modify: `src/stats.rs:1`
- Test: existing store tests

- [ ] **Step 1: Rename inside store.rs only**

```bash
# store.rs is now entirely about accounts — every remaining user_id in it
# is a column or parameter that just moved.
sed -i '' 's/\buser_id\b/account_id/g' src/store.rs
```

Then **read the diff** and revert any hit inside `LEGACY_SCHEMA`,
`LEGACY_USER_IDS`, `legacy_db()`, `STEP_3_UNCONSTRAINED`,
`STEP_4_REBUILDS` and `STEP_5_DELIVERIES` — those name the *old* columns on
purpose and must keep saying `user_id`.

```bash
git diff src/store.rs | head -200
```

- [ ] **Step 2: Fix the reminder struct**

`Reminder` (`src/store.rs:167-175`) carries `chat_id`. Replace:

```rust
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
```

and update `row_to_reminder`, `create_reminder`, `list_reminders`,
`due_reminders` and `cancel_reminder` to select and bind
`channel`/`address` in place of `chat_id`.

- [ ] **Step 3: Add the identity resolver**

```rust
    /// The account behind a Telegram user, creating one the first time.
    ///
    /// One statement per branch and both inside the same lock, so two
    /// updates from the same person cannot mint two accounts.
    pub fn account_for_telegram(&self, telegram_id: i64) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let key = telegram_id.to_string();
        let mut stmt = conn
            .prepare("SELECT account_id FROM identities WHERE kind = 'telegram' AND external_id = ?")?;
        let found: Option<i64> = stmt.query_map(params![key], |r| r.get(0))?.next().transpose()?;
        drop(stmt);
        if let Some(id) = found {
            return Ok(id);
        }
        let account_id: i64 = conn.query_row(
            "INSERT INTO accounts (id) VALUES (nextval('accounts_id_seq')) RETURNING id",
            [], |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO identities (account_id, kind, external_id) VALUES (?, 'telegram', ?)",
            params![account_id, key],
        )?;
        Ok(account_id)
    }
```

- [ ] **Step 4: Build and fix what the compiler names**

Run: `cargo build 2>&1 | head -40`
Expected: errors in `src/bot.rs`, `src/agent.rs`, `src/stats.rs` and
`src/tools/*.rs`. Rename the field on each tool struct listed below from
`user_id` to `account_id` — the value passed into it changes meaning in
Task 9, and leaving the old name would make that invisible:

`src/tools/memory.rs:26,85` · `src/tools/purchases.rs:18,76` ·
`src/tools/duffel.rs:27,174` · `src/tools/reminders.rs:39,108,145` ·
`src/tools/trips.rs:515,770,919,1084,1147,1212,1308,1372`

- [ ] **Step 5: Run the suite**

Run: `cargo test`
Expected: PASS, 412 plus the new tests. If a store test fails, it is
asserting on a Telegram id where it should now assert on an account id.

- [ ] **Step 6: Commit**

```bash
git add src/store.rs src/stats.rs src/tools/
git commit -m "refactor: the store and its tools speak account ids"
```

---

## Task 9: The adapter resolves the account

**Files:**
- Modify: `src/bot.rs` — `handle_text`, `handle_photo`, `note_chat`, `log_request`, `handle_start`
- Modify: `src/agent.rs:645` — `build_agent`

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn the_same_telegram_user_always_resolves_to_one_account() {
        let (s, _d) = test_store();
        let a = s.account_for_telegram(4242).unwrap();
        let b = s.account_for_telegram(4242).unwrap();
        assert_eq!(a, b);
        let c = s.account_for_telegram(4343).unwrap();
        assert_ne!(a, c);
    }
```

- [ ] **Step 2: Run it**

Run: `cargo test the_same_telegram_user_always_resolves_to_one_account`
Expected: PASS (Task 8 added the method) — this pins it against later edits.

- [ ] **Step 3: Rename `build_agent`'s parameter**

In `src/agent.rs:645`, change `user_id: i64` to `account_id: i64` and update
every `user_id` inside the body to `account_id`. Its 14 tool constructions
already use the renamed field, so this is the matching half.

- [ ] **Step 4: Resolve once per update in bot.rs**

`sender_id(msg)` stays a Telegram id — the gate, the founder list and
`app.members` are all Telegram-keyed and must not be renamed. Add the
resolution immediately before the agent runs, in `handle_text` and
`handle_photo`:

```rust
    let account_id = {
        let store = app.deps.store.clone();
        match blocking(move || store.account_for_telegram(user_id)).await {
            Ok(id) => id,
            Err(e) => {
                tracing::error!(error = %e, user_id, "could not resolve an account");
                return Ok(());
            }
        }
    };
```

Pass `account_id` where `user_id` was passed to `build_agent`,
`log_request` and `over_daily_cap`.

- [ ] **Step 5: Point note_chat at deliveries**

Replace the body of `note_chat` (`src/bot.rs:710`) so it writes a delivery
rather than a `user_chats` row:

```rust
fn note_chat(app: &Arc<App>, user_id: i64, chat_id: i64) {
    let store = app.deps.store.clone();
    tokio::spawn(async move {
        let r = tokio::task::spawn_blocking(move || {
            let account_id = store.account_for_telegram(user_id)?;
            store.note_delivery(account_id, "telegram", &chat_id.to_string())
        })
        .await;
        if let Err(e) = r {
            tracing::warn!(error = %e, "could not record where to reach this person");
        }
    });
}
```

and add to `Store`:

```rust
    pub fn note_delivery(&self, account_id: i64, channel: &str, address: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO deliveries (account_id, channel, address) VALUES (?, ?, ?)
             ON CONFLICT (account_id, channel)
             DO UPDATE SET address = excluded.address, updated_at = current_timestamp",
            params![account_id, channel, address],
        )?;
        Ok(())
    }
```

- [ ] **Step 6: Run the suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/bot.rs src/agent.rs src/store.rs
git commit -m "feat: the adapter turns a telegram id into an account before the agent runs"
```

---

## Task 10: The scheduler sends to a reminder's own address

**Files:**
- Modify: `src/scheduler.rs:51`

- [ ] **Step 1: Change the send**

```rust
        let Ok(chat) = reminder.address.parse::<i64>() else {
            tracing::error!(reminder_id = reminder.id, address = %reminder.address,
                "unparseable delivery address; skipping reminder");
            continue;
        };
        match bot.send_message(ChatId(chat), text).await {
```

- [ ] **Step 2: Run the suite**

Run: `cargo test`
Expected: PASS — the four `advance_from` tests are unaffected.

- [ ] **Step 3: Commit**

```bash
git add src/scheduler.rs
git commit -m "fix: a reminder is delivered to the address it was created with"
```

---

## Task 11: Conversations in the database

**Files:**
- Modify: `src/store.rs`
- Test: `src/store.rs` (`mod tests`)

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn a_conversation_round_trips_and_is_scoped() {
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();

        let direct = s.current_conversation(a, "direct").unwrap();
        let group = s.current_conversation(a, "telegram:-100").unwrap();
        assert_ne!(direct, group, "a group must not share the private thread");

        s.append_message(direct, r#"{"role":"user","content":"hi"}"#).unwrap();
        s.append_message(direct, r#"{"role":"assistant","content":"hello"}"#).unwrap();

        let bodies = s.conversation_messages(direct, 20).unwrap();
        assert_eq!(bodies.len(), 2);
        assert!(bodies[0].contains("hi"), "oldest first");

        assert!(s.conversation_messages(group, 20).unwrap().is_empty());
    }

    #[test]
    fn a_conversation_returns_only_the_last_n_messages_oldest_first() {
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let c = s.current_conversation(a, "direct").unwrap();
        for i in 0..25 {
            s.append_message(c, &format!(r#"{{"n":{i}}}"#)).unwrap();
        }
        let bodies = s.conversation_messages(c, 20).unwrap();
        assert_eq!(bodies.len(), 20);
        assert!(bodies[0].contains(r#""n":5"#), "should drop the oldest five");
        assert!(bodies[19].contains(r#""n":24"#), "and end at the newest");
    }
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test a_conversation_round_trips_and_is_scoped`
Expected: FAIL — `no method named 'current_conversation'`

- [ ] **Step 3: Implement**

```rust
    /// The live conversation for this account in this scope, started if
    /// there is none or the last one has gone quiet for longer than `ttl`.
    pub fn current_conversation(&self, account_id: i64, scope: &str) -> Result<i64> {
        self.current_conversation_after(account_id, scope, 0)
    }

    /// `ttl_secs` of 0 means "never expire", which is what the plain
    /// `current_conversation` wants; the caller with a TTL passes it in so
    /// expiry stays one rule in one place.
    pub fn current_conversation_after(&self, account_id: i64, scope: &str, ttl_secs: i64) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        // The cast is load-bearing: `current_timestamp` is TIMESTAMPTZ and
        // DuckDB v1.5.1 has no `TIMESTAMPTZ - INTERVAL` overload, so the
        // uncast form fails with "No function matches the given name and
        // argument types '-(TIMESTAMP WITH TIME ZONE, INTERVAL)'". Verified.
        let mut stmt = conn.prepare(
            "SELECT id FROM conversations WHERE account_id = ? AND scope = ?
             AND (? = 0 OR updated_at > CAST(current_timestamp AS TIMESTAMP) - to_seconds(?))
             ORDER BY updated_at DESC LIMIT 1",
        )?;
        let found: Option<i64> = stmt
            .query_map(params![account_id, scope, ttl_secs, ttl_secs], |r| r.get(0))?
            .next()
            .transpose()?;
        drop(stmt);
        if let Some(id) = found {
            return Ok(id);
        }
        Ok(conn.query_row(
            "INSERT INTO conversations (id, account_id, scope)
             VALUES (nextval('conversations_id_seq'), ?, ?) RETURNING id",
            params![account_id, scope],
            |r| r.get(0),
        )?)
    }

    pub fn append_message(&self, conversation_id: i64, body: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO messages (id, conversation_id, position, body)
             VALUES (nextval('messages_id_seq'), ?,
                     coalesce((SELECT max(position) + 1 FROM messages WHERE conversation_id = ?), 0), ?)",
            params![conversation_id, conversation_id, body],
        )?;
        conn.execute(
            "UPDATE conversations SET updated_at = current_timestamp WHERE id = ?",
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
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn set_pending_draft(&self, conversation_id: i64, draft: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE conversations SET pending_draft = ? WHERE id = ?",
            params![draft, conversation_id],
        )?;
        Ok(())
    }

    pub fn pending_draft(&self, conversation_id: i64) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT pending_draft FROM conversations WHERE id = ?")?;
        let row: Option<Option<String>> =
            stmt.query_map(params![conversation_id], |r| r.get(0))?.next().transpose()?;
        Ok(row.flatten())
    }
```

- [ ] **Step 4: Run both tests**

Run: `cargo test a_conversation`
Expected: both PASS

- [ ] **Step 5: Prove the scoping guard**

Change `AND scope = ?` to `AND ? = ?` in `current_conversation_after`, run
`cargo test a_conversation_round_trips_and_is_scoped`, and confirm it fails
on `assert_ne!`. Restore it.

- [ ] **Step 6: Commit**

```bash
git add src/store.rs
git commit -m "feat: a conversation is rows, not a map entry"
```

---

## Task 12: History loads from and saves to the store

**Files:**
- Modify: `src/bot.rs:21-46` (`App`), `ChatSession` handling, `run_agent`
- Test: `src/bot.rs` (`mod tests`)

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn history_survives_being_dropped_and_reloaded() {
        let (s, _d) = crate::store::tests::test_store();
        let a = s.account_for_telegram(11).unwrap();
        let c = s.current_conversation(a, "direct").unwrap();

        let original = vec![LlmMessage::user("cheapest beans"), LlmMessage::assistant("here")];
        save_history(&s, c, &original).unwrap();

        let loaded = load_history(&s, c, HISTORY_CAP).unwrap();
        assert_eq!(loaded, original, "a reloaded history must be identical");
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test history_survives_being_dropped_and_reloaded`
Expected: FAIL — `cannot find function 'save_history'`

- [ ] **Step 3: Implement the two helpers**

Add to `src/bot.rs`:

```rust
/// Rewrites a conversation's messages to match `history`.
///
/// A whole rewrite rather than an append: `trim_history` may drop messages
/// from the front, and the stored thread has to be what the agent will
/// actually be sent next time, not a growing log that disagrees with it.
fn save_history(store: &Store, conversation_id: i64, history: &[LlmMessage]) -> anyhow::Result<()> {
    store.replace_messages(
        conversation_id,
        &history
            .iter()
            .map(serde_json::to_string)
            .collect::<std::result::Result<Vec<_>, _>>()?,
    )
}

/// Messages a provider will accept, oldest first. A row that no longer
/// deserializes — because rig changed shape under us — is dropped rather
/// than fatal: losing context is survivable, refusing to answer is not.
fn load_history(store: &Store, conversation_id: i64, cap: usize) -> anyhow::Result<Vec<LlmMessage>> {
    let bodies = store.conversation_messages(conversation_id, cap)?;
    let mut out = Vec::with_capacity(bodies.len());
    for body in bodies {
        match serde_json::from_str::<LlmMessage>(&body) {
            Ok(m) => out.push(m),
            Err(e) => tracing::warn!(error = %e, "dropping an unreadable stored message"),
        }
    }
    Ok(out)
}
```

Add to `Store` in `src/store.rs`:

```rust
    /// Replaces a conversation's messages wholesale, in one lock so a reader
    /// never sees the thread half-written.
    pub fn replace_messages(&self, conversation_id: i64, bodies: &[String]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<()> {
            conn.execute("DELETE FROM messages WHERE conversation_id = ?", params![conversation_id])?;
            for (i, body) in bodies.iter().enumerate() {
                conn.execute(
                    "INSERT INTO messages (id, conversation_id, position, body)
                     VALUES (nextval('messages_id_seq'), ?, ?, ?)",
                    params![conversation_id, i as i64, body],
                )?;
            }
            conn.execute(
                "UPDATE conversations SET updated_at = current_timestamp WHERE id = ?",
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
```

Two things the compiler will ask for: mark the store's test module
`pub(crate) mod tests` at `src/store.rs:1324` so `test_store` is reachable
from `bot.rs`, and add `use crate::store::Store;` to `src/bot.rs` — it
currently reaches the store only through `app.deps.store` and so never named
the type.

- [ ] **Step 4: Run it**

Run: `cargo test history_survives_being_dropped_and_reloaded`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/bot.rs src/store.rs
git commit -m "feat: a conversation can be written down and read back"
```

---

## Task 13: Retire the in-memory history

**Files:**
- Modify: `src/bot.rs:21-46` (`App`), `src/bot.rs:66-99` (`ChatSession`), `run_agent`

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn a_scope_string_separates_a_group_from_a_private_chat() {
        assert_eq!(conversation_scope(4242, 4242), "direct");
        assert_eq!(conversation_scope(-100123, 4242), "telegram:-100123");
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test a_scope_string_separates_a_group_from_a_private_chat`
Expected: FAIL — `cannot find function 'conversation_scope'`

- [ ] **Step 3: Implement**

```rust
/// In a 1:1 chat Telegram makes the chat id equal the user id, and that
/// thread is the one the web app will share. Anywhere else is a room with
/// other people in it and keeps its own history.
fn conversation_scope(chat_id: i64, user_id: i64) -> String {
    if chat_id == user_id {
        "direct".to_string()
    } else {
        format!("telegram:{chat_id}")
    }
}
```

- [ ] **Step 4: Replace the DashMap read/write in `run_agent`**

Where `run_agent` currently takes `history` from `app.chats` and writes it
back after `trim_history` (`src/bot.rs:1737-1756`), load and save instead:

```rust
    let scope = conversation_scope(chat_id, user_id);
    let conversation_id = {
        let store = app.deps.store.clone();
        let scope = scope.clone();
        blocking(move || {
            store.current_conversation_after(account_id, &scope, SESSION_TTL.as_secs() as i64)
        })
        .await?
    };
    let mut history = {
        let store = app.deps.store.clone();
        blocking(move || load_history(&store, conversation_id, HISTORY_CAP)).await?
    };
```

and after `trim_history(&mut history, HISTORY_CAP);`:

```rust
    let store = app.deps.store.clone();
    let to_save = history.clone();
    if let Err(e) = blocking(move || save_history(&store, conversation_id, &to_save)).await {
        tracing::warn!(error = %e, "could not save the conversation");
    }
```

- [ ] **Step 5: Delete the history field**

Remove `history` from `ChatSession` (`src/bot.rs:67`) and every read of it.
`pending_draft` and `last_seen` stay for now — the draft moves to
`conversations.pending_draft` and `last_seen` is replaced by
`conversations.updated_at`, so once nothing reads `ChatSession` at all,
delete the struct and the `chats` field on `App`.

Keep `take_expired_session`'s behaviour: `current_conversation_after` with
`SESSION_TTL` now performs the expiry, so the aged-out history that
`continues_previous` inspects must come from `load_history` on the *previous*
conversation. Fetch it explicitly before starting a new one:

```rust
    let previous = {
        let store = app.deps.store.clone();
        let scope = scope.clone();
        blocking(move || store.current_conversation_after(account_id, &scope, 0)).await?
    };
    let aged_out = previous != conversation_id;
```

- [ ] **Step 6: Run everything**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/bot.rs
git commit -m "feat: a conversation survives a restart"
```

---

## Task 14: Deploy

- [ ] **Step 1: Back up the live database from inside the container**

The host's Python DuckDB is 1.4.5 and the container's engine is v1.5.1 —
**older, not newer**. Do not open the file from the host at all.

```bash
docker compose exec -T scout sh -c 'cp /data/scout.duckdb /data/scout.duckdb.pre-accounts'
docker compose exec -T scout sh -c 'ls -la /data/'
```

Confirm the copy exists and is the same size before going on.

- [ ] **Step 2: Deploy**

```bash
docker compose up -d --build
```

- [ ] **Step 3: Confirm the migration ran once**

```bash
docker compose logs --tail 40 scout | grep -E 'applied migration step|who may talk|scout is up'
```

Expected: five `applied migration step` lines (1 through 5), then
`who may talk to this bot founders=8 members=1 daily_cap=20`, then
`scout is up`. Restart count must stay where it was:

```bash
docker inspect scout-scout-1 --format 'restarts={{.RestartCount}}'
```

- [ ] **Step 4: Confirm a restart applies nothing**

```bash
docker compose restart scout
docker compose logs --tail 20 scout | grep -c 'applied migration step'
```

Expected: `0`. A second application would double the accounts.

- [ ] **Step 5: Send the bot a message, then restart it, then ask a follow-up**

The follow-up should be understood in context. That is the phase-one
feature: before this, a restart lost the thread.

---

## Rollback

The migration is one-way. If it goes wrong:

```bash
docker compose stop scout
docker compose exec -T scout sh -c 'cp /data/scout.duckdb.pre-accounts /data/scout.duckdb'
git revert <merge commit>
docker compose up -d --build
```

The backup from Task 14 Step 1 is the only rollback path, which is why it is
a numbered step and not advice.
