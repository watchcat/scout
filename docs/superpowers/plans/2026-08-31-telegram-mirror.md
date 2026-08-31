# Telegram Mirror Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A toggle on `/chat` that sends the current thread to the reader's Telegram chat — all of it when switched on, then each new exchange as it completes.

**Architecture:** A durable `outbox` table that the web side enqueues into and the Telegram side drains. Neither crate calls the other; they meet at the table. Rows carry a `turn_key` (a sha256 of conversation, role and text) under a `UNIQUE` constraint, which makes enqueueing idempotent — so backfill, live mirroring, and toggling off and on are all the same operation. The Telegram channel writes its *own* exchanges into the outbox already marked sent, which is what stops a backfill echoing Telegram's messages back at it.

**Tech Stack:** Rust, DuckDB (single-writer, behind a `Mutex`), axum 0.8, teloxide 0.17, `sha2`, `tokio::sync::Notify`. Tests are `cargo test` and `node --test`.

**Spec:** `docs/superpowers/specs/2026-08-31-telegram-mirror-design.md`

---

## Things to know before starting

**This repository is deliberately not rustfmt-formatted. Never run `cargo fmt`.** Match the hand-formatting of the file you are editing.

**Clippy needs the rustup toolchain.** A stale clippy from the nix store shadows it and fails with `E0514`. Always run:

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo clippy --workspace --all-targets
```

**Adding a table needs no versioned migration step.** `Store::open` runs `execute_batch(MIGRATIONS)` on *every* open, and `MIGRATIONS` is all `CREATE TABLE IF NOT EXISTS`. The numbered `steps()` exist only for transforms of existing data. Two new empty tables need no step, and `schema_version` stays at 6.

**Comments do not belong in source-scan tests.** Two tests written today matched their own explanatory prose and stayed green when the code under test was broken. If you write a test that greps source, scope it to a specific declaration.

**Test helper:** `crate::store::tests::test_store()` returns `(Store, TempDir)`. Keep the `TempDir` bound or the database is deleted underneath you.

---

## File structure

| File | Responsibility |
|---|---|
| `crates/scout-core/src/store.rs` | Two new tables in `MIGRATIONS`; the six synchronous outbox/setting queries. |
| `crates/scout-core/src/mirror.rs` | **New.** `turn_key`, the `Pending` type, and the async wrappers channels call. |
| `crates/scout-core/src/core.rs` | The `Notify` that wakes the drain. |
| `crates/scout-core/src/lib.rs` | `pub mod mirror;` |
| `crates/scout-core/Cargo.toml` | `sha2` dependency. |
| `crates/scout-web/src/routes/chat.rs` | `POST /chat/mirror`; enqueue after a completed run; divider on reset. |
| `crates/scout-web/src/chat.html` | The header toggle. |
| `crates/scout-web/src/chat.js` | Its click handler. |
| `crates/scout-telegram/src/mirror.rs` | **New.** The `Sink` trait and the drain loop. |
| `crates/scout-telegram/src/bot.rs` | Record Telegram's own exchanges as delivered. |
| `crates/scout-telegram/src/main.rs` | Spawn the drain. |

---

## Task 1: The two tables

**Files:**
- Modify: `crates/scout-core/src/store.rs` (the `MIGRATIONS` const, near the `deliveries` table at line ~150)

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `crates/scout-core/src/store.rs`:

```rust
    #[test]
    fn a_fresh_store_has_somewhere_to_queue_a_mirror() {
        // Both tables are pure additions with nothing to migrate, so they
        // live in MIGRATIONS alone — `open` runs that batch every time, on
        // existing databases as well as new ones, and the numbered steps
        // exist only for transforms.
        let (s, _d) = test_store();
        let conn = s.conn.lock().unwrap();
        for table in ["outbox", "mirrored_accounts"] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM information_schema.tables WHERE table_name = ?",
                    params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "{table} is missing");
        }
    }
```

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p scout-core --lib a_fresh_store_has_somewhere_to_queue
```

Expected: FAIL, `outbox is missing`.

- [ ] **Step 3: Add the tables**

In `crates/scout-core/src/store.rs`, inside the `MIGRATIONS` string, immediately after the `deliveries` table definition:

```sql
CREATE SEQUENCE IF NOT EXISTS outbox_id_seq;
-- Messages waiting to be mirrored to a channel the reader also uses.
--
-- A table rather than an in-memory queue because the web crate cannot reach
-- the Telegram bot — the dependency runs scout-telegram -> scout-web ->
-- scout-core — and because an in-memory queue loses a half-sent backfill on
-- every deploy.
--
-- `turn_key` is what makes enqueueing idempotent, and it exists because no
-- stored message has a stable identity: `replace_messages` deletes and
-- reinserts the whole conversation on every save and renumbers `position`
-- from zero, so "mirrored up to here" cannot be a pointer. "Have I already
-- sent this turn" can be answered; "how far did I get" cannot.
--
-- A row with `sent_at` already set was never going to be sent: that is how
-- the Telegram channel records its own messages so a backfill does not echo
-- them back at it.
CREATE TABLE IF NOT EXISTS outbox (
    id         BIGINT PRIMARY KEY DEFAULT nextval('outbox_id_seq'),
    account_id BIGINT NOT NULL,
    channel    TEXT NOT NULL,
    address    TEXT NOT NULL,
    body       TEXT NOT NULL,
    turn_key   TEXT NOT NULL,
    attempts   BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    sent_at    TIMESTAMP,
    UNIQUE (account_id, turn_key)
);
-- Who wants their browser thread mirrored. Presence is the setting: a row
-- means on, no row means off, and there is no boolean that can fall out of
-- step with itself.
CREATE TABLE IF NOT EXISTS mirrored_accounts (
    account_id BIGINT PRIMARY KEY,
    enabled_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
```

- [ ] **Step 4: Run it and watch it pass**

```bash
cargo test -p scout-core --lib a_fresh_store_has_somewhere_to_queue
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/store.rs
git commit -m "feat: somewhere to queue a mirrored turn"
```

---

## Task 2: `turn_key`

**Files:**
- Create: `crates/scout-core/src/mirror.rs`
- Modify: `crates/scout-core/src/lib.rs`, `crates/scout-core/Cargo.toml`

- [ ] **Step 1: Add the dependency**

In `crates/scout-core/Cargo.toml`, under `[dependencies]`, after `base64 = "0.22"`:

```toml
sha2 = "0.11"
```

`scout-web` already depends on `sha2 = "0.11"` — 0.11, not 0.13; the `hmac = "0.13"` on the line above it is a different crate. Use 0.11 so the workspace resolves one copy rather than a third.

- [ ] **Step 2: Write the failing test**

Create `crates/scout-core/src/mirror.rs` containing only:

```rust
//! Sending a browser thread to the reader's Telegram chat.

#[cfg(test)]
mod tests {
    use super::*;
    use scout_api::Role;

    #[test]
    fn the_same_turn_always_has_the_same_key() {
        // The whole idempotence argument rests on this. If the key moved --
        // under a toolchain upgrade, say -- every thread would be sent again.
        assert_eq!(turn_key(7, Role::You, "cheapest beans"), turn_key(7, Role::You, "cheapest beans"));
        // And it is a fixed hex digest, not a hash whose stability is a
        // promise nobody made. `DefaultHasher` explicitly is not stable
        // across releases, which is why it is not used here.
        assert_eq!(turn_key(7, Role::You, "cheapest beans").len(), 64);
        assert!(turn_key(7, Role::You, "cheapest beans").chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_different_turn_has_a_different_key() {
        // Role, because "cheapest beans" asked and "cheapest beans" answered
        // are two turns. Conversation, because the same question in a new
        // thread is a new turn and has to be mirrored again.
        assert_ne!(turn_key(7, Role::You, "beans"), turn_key(7, Role::Scout, "beans"));
        assert_ne!(turn_key(7, Role::You, "beans"), turn_key(8, Role::You, "beans"));
        assert_ne!(turn_key(7, Role::You, "beans"), turn_key(7, Role::You, "rice"));
    }

    #[test]
    fn neighbouring_fields_cannot_be_confused_for_one_another() {
        // Stated as what it is. Measured: with this layout the
        // concatenation is already injective — the id is digits, a role
        // starts with a letter, and neither role name is a prefix of the
        // other — so these differ with or without the separators. An
        // earlier draft claimed the separators were what made them differ,
        // and the test passed under its own mutation as a result.
        assert_ne!(turn_key(1, Role::You, "23"), turn_key(12, Role::You, "3"));
        assert_ne!(turn_key(1, Role::Scout, "x"), turn_key(1, Role::You, "scoutx"));
    }
}
```

- [ ] **Step 3: Run it and watch it fail**

```bash
cargo test -p scout-core --lib mirror::
```

Expected: FAIL to compile, `cannot find function turn_key`.

- [ ] **Step 4: Write the implementation**

At the top of `crates/scout-core/src/mirror.rs`, below the module doc:

```rust
use scout_api::Role;
use sha2::{Digest, Sha256};

/// What this channel is called in the outbox. One channel today; named
/// rather than written inline so a second one is a value, not an edit.
pub const TELEGRAM: &str = "telegram";

/// A stable name for one turn of one conversation.
///
/// This is what replaces a watermark. No stored message has a stable
/// identity — `replace_messages` deletes and reinserts the whole
/// conversation on every save and renumbers `position` from zero, and
/// `trim_history` drops messages off the front — so "mirrored up to here"
/// cannot be a pointer into `messages`. "Have I already sent this turn" is
/// answerable regardless, and that single change makes backfill, live
/// mirroring, and toggling the feature off and on the same operation.
///
/// sha2 rather than `DefaultHasher`: the standard hasher is explicitly not
/// stable across Rust releases, and a key that changed under a toolchain
/// upgrade would re-send every thread the reader had already read.
///
/// The `\x1f` separators are insurance against a change to these fields,
/// not a fix for a collision that exists today. Measured: with this exact
/// layout the concatenation is already injective, because the id is digits,
/// a role starts with a letter, and neither role name is a prefix of the
/// other — so `1|you|23` and `12|you|3` cannot be confused even unseparated.
/// They stay because hashing concatenated fields without them is a habit
/// that bites the first time a field stops being digits.
pub fn turn_key(conversation_id: i64, role: Role, text: &str) -> String {
    let role = match role {
        Role::You => "you",
        Role::Scout => "scout",
    };
    let mut hasher = Sha256::new();
    hasher.update(conversation_id.to_string().as_bytes());
    hasher.update(b"\x1f");
    hasher.update(role.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}
```

In `crates/scout-core/src/lib.rs`, add `pub mod mirror;` between `pub mod links;` and `pub mod run;`.

- [ ] **Step 5: Run it and watch it pass**

```bash
cargo test -p scout-core --lib mirror::
```

Expected: PASS, 3 tests.

- [ ] **Step 6: Do not mutation-check the separators**

They are not load-bearing with this field layout and no test will go red when
they are removed — that is stated in the doc comment rather than pretended
otherwise. Mutation-check `a_different_turn_has_a_different_key` instead by
dropping the `role` from the hash: it must fail.

- [ ] **Step 7: Commit**

```bash
git add crates/scout-core/src/mirror.rs crates/scout-core/src/lib.rs crates/scout-core/Cargo.toml Cargo.lock
git commit -m "feat: a stable name for one turn of one conversation"
```

---

## Task 3: The store queries

**Files:**
- Modify: `crates/scout-core/src/store.rs`

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/scout-core/src/store.rs`:

```rust
    #[test]
    fn enqueueing_the_same_turn_twice_leaves_one_row() {
        // Backfill and the live path both enqueue, and toggling off and on
        // backfills again. All three lean on this being a no-op.
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        assert!(s.enqueue_mirror(a, "telegram", "11", "hello", "key-1", false).unwrap());
        assert!(!s.enqueue_mirror(a, "telegram", "11", "hello", "key-1", false).unwrap());
        assert_eq!(s.pending_mirror("telegram", 10).unwrap().len(), 1);
    }

    #[test]
    fn a_turn_the_channel_already_delivered_is_never_pending() {
        // This is the echo guarantee. The browser and a 1:1 Telegram chat
        // share conversation scope "direct", so backfilling the thread would
        // send Telegram its own messages back. The channel that handled a
        // turn records it as delivered, and the backfill then skips it
        // because "already in the ledger" and "already sent" are one fact.
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        assert!(s.enqueue_mirror(a, "telegram", "11", "from telegram", "key-2", true).unwrap());
        assert!(s.pending_mirror("telegram", 10).unwrap().is_empty());
        // And the backfill's later attempt at the same turn changes nothing.
        assert!(!s.enqueue_mirror(a, "telegram", "11", "from telegram", "key-2", false).unwrap());
        assert!(s.pending_mirror("telegram", 10).unwrap().is_empty());
    }

    #[test]
    fn pending_rows_come_back_oldest_first_and_marking_them_clears_them() {
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        s.enqueue_mirror(a, "telegram", "11", "first", "k1", false).unwrap();
        s.enqueue_mirror(a, "telegram", "11", "second", "k2", false).unwrap();
        let due = s.pending_mirror("telegram", 10).unwrap();
        assert_eq!(due.iter().map(|r| r.body.as_str()).collect::<Vec<_>>(), ["first", "second"]);
        s.mark_mirror_sent(due[0].id).unwrap();
        let due = s.pending_mirror("telegram", 10).unwrap();
        assert_eq!(due.iter().map(|r| r.body.as_str()).collect::<Vec<_>>(), ["second"]);
    }

    #[test]
    fn a_row_is_abandoned_after_five_attempts() {
        // The reminder path can retry forever safely because dates bound it.
        // This one has no such bound: block the bot and an uncapped retry
        // loops until a human notices.
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        s.enqueue_mirror(a, "telegram", "11", "doomed", "k1", false).unwrap();
        let id = s.pending_mirror("telegram", 10).unwrap()[0].id;
        for _ in 0..4 {
            s.mark_mirror_failed(id).unwrap();
            assert_eq!(s.pending_mirror("telegram", 10).unwrap().len(), 1, "gave up too early");
        }
        s.mark_mirror_failed(id).unwrap();
        assert!(s.pending_mirror("telegram", 10).unwrap().is_empty(), "retried forever");
    }

    #[test]
    fn the_mirror_setting_is_the_presence_of_a_row() {
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        assert!(!s.mirror_enabled(a).unwrap());
        s.set_mirror(a, true).unwrap();
        assert!(s.mirror_enabled(a).unwrap());
        // Enabling twice is not an error -- the page can post it twice.
        s.set_mirror(a, true).unwrap();
        assert!(s.mirror_enabled(a).unwrap());
        s.set_mirror(a, false).unwrap();
        assert!(!s.mirror_enabled(a).unwrap());
    }
```

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p scout-core --lib store::tests::enqueueing_the_same_turn
```

Expected: FAIL to compile, `no method named enqueue_mirror`.

- [ ] **Step 3: Write the implementation**

Add to the `impl Store` block in `crates/scout-core/src/store.rs`, after `replace_messages`:

```rust
    /// Queues one turn for a channel, or does nothing if it is already
    /// known. Returns whether a row was written.
    ///
    /// `delivered` is how a channel records a turn it has already shown the
    /// reader: the row is inserted with `sent_at` set, so it occupies the
    /// key and is never dispatched.
    ///
    /// Check-then-insert rather than `ON CONFLICT`, because the store holds
    /// one mutex and is the only writer, so the pair is atomic here in a way
    /// it would not be over a network. The `UNIQUE` constraint stays as a
    /// backstop against a second writer nobody has added yet.
    pub fn enqueue_mirror(
        &self,
        account_id: i64,
        channel: &str,
        address: &str,
        body: &str,
        turn_key: &str,
        delivered: bool,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let known: i64 = conn.query_row(
            "SELECT count(*) FROM outbox WHERE account_id = ? AND turn_key = ?",
            params![account_id, turn_key],
            |r| r.get(0),
        )?;
        if known > 0 {
            return Ok(false);
        }
        // `now()` in the statement rather than a bound timestamp: `duckdb`
        // is built here without its `chrono` feature, so a `NaiveDateTime`
        // has no `ToSql`, and every other write in this file dates itself
        // the same way.
        let sql = if delivered {
            "INSERT INTO outbox (id, account_id, channel, address, body, turn_key, sent_at)
             VALUES (nextval('outbox_id_seq'), ?, ?, ?, ?, ?, now())"
        } else {
            "INSERT INTO outbox (id, account_id, channel, address, body, turn_key)
             VALUES (nextval('outbox_id_seq'), ?, ?, ?, ?, ?)"
        };
        conn.execute(sql, params![account_id, channel, address, body, turn_key])?;
        Ok(true)
    }

    /// Turns still waiting to go out on a channel, oldest first.
    ///
    /// Oldest first because a thread delivered out of order is worse than
    /// one delivered late. Rows past [`MIRROR_ATTEMPTS`] are left behind
    /// rather than returned.
    pub fn pending_mirror(&self, channel: &str, limit: usize) -> Result<Vec<PendingMirror>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, account_id, address, body FROM outbox
             WHERE channel = ? AND sent_at IS NULL AND attempts < ?
             ORDER BY id LIMIT ?",
        )?;
        let rows = stmt.query_map(params![channel, MIRROR_ATTEMPTS, limit as i64], |r| {
            Ok(PendingMirror {
                id: r.get(0)?,
                account_id: r.get(1)?,
                address: r.get(2)?,
                body: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// It arrived. The row stays as the ledger entry that stops it being
    /// sent again by a later backfill.
    pub fn mark_mirror_sent(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE outbox SET sent_at = now() WHERE id = ?", params![id])?;
        Ok(())
    }

    /// It did not arrive. One more attempt spent; at [`MIRROR_ATTEMPTS`] the
    /// row stops being returned by `pending_mirror`.
    pub fn mark_mirror_failed(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE outbox SET attempts = attempts + 1 WHERE id = ?", params![id])?;
        Ok(())
    }

    pub fn mirror_enabled(&self, account_id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM mirrored_accounts WHERE account_id = ?",
            params![account_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Presence is the setting, so turning it on twice is not an error and
    /// turning it off is a delete.
    pub fn set_mirror(&self, account_id: i64, on: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        if on {
            conn.execute(
                "INSERT INTO mirrored_accounts (account_id) SELECT ?
                 WHERE NOT EXISTS (SELECT 1 FROM mirrored_accounts WHERE account_id = ?)",
                params![account_id, account_id],
            )?;
        } else {
            conn.execute("DELETE FROM mirrored_accounts WHERE account_id = ?", params![account_id])?;
        }
        Ok(())
    }
```

Add near the top of `crates/scout-core/src/store.rs`, beside the other consts:

```rust
/// How many times a mirrored message is retried before it is left alone.
///
/// The reminder path retries indefinitely and that is safe, because a date
/// bounds it. An outbox row has no such bound: a reader who blocks the bot
/// would otherwise be retried against forever.
const MIRROR_ATTEMPTS: i64 = 5;
```

And beside the other row structs, above `impl Store`:

```rust
/// One thing still waiting to go out.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingMirror {
    pub id: i64,
    pub account_id: i64,
    pub address: String,
    pub body: String,
}
```

- [ ] **Step 4: Run them and watch them pass**

```bash
cargo test -p scout-core --lib store::tests
```

Expected: PASS, including the five new tests.

- [ ] **Step 5: Mutation-check the echo guarantee**

Change `enqueue_mirror` so `sent_at` is always `None` (ignore `delivered`). Run `cargo test -p scout-core --lib store::tests::a_turn_the_channel_already_delivered` and confirm it fails. Restore.

- [ ] **Step 6: Commit**

```bash
git add crates/scout-core/src/store.rs
git commit -m "feat: an outbox that cannot send the same turn twice"
```

---

## Task 4: The core module and the wake-up

**Files:**
- Modify: `crates/scout-core/src/mirror.rs`, `crates/scout-core/src/core.rs`

- [ ] **Step 1: Add the notify to `Core`**

In `crates/scout-core/src/core.rs`, change the struct:

```rust
pub struct Core {
    pub(crate) cfg: Config,
    pub(crate) deps: AgentDeps,
    /// Woken when something is queued for a channel to deliver.
    ///
    /// The reminder tick runs every fifteen minutes, which is right for a
    /// date-scheduled reminder and far too slow for a thread someone is
    /// about to read on their phone. The tick stays as a floor, so a missed
    /// signal is a delay rather than a lost mirror.
    pub(crate) mirror_wake: std::sync::Arc<tokio::sync::Notify>,
}
```

In `Core::start`, add `mirror_wake: std::sync::Arc::new(tokio::sync::Notify::new()),` to the `Self { .. }` it returns.

Add these two methods to `impl Core`, beside `due_deliveries`:

```rust
    /// Something is waiting. Wakes a drain that is asleep; harmless if none
    /// is listening.
    pub fn wake_mirror(&self) {
        self.mirror_wake.notify_one();
    }

    /// Resolves when something has been queued.
    pub async fn mirror_waiting(&self) {
        self.mirror_wake.notified().await;
    }
```

- [ ] **Step 2: Write the failing test**

Add to `mod tests` in `crates/scout-core/src/mirror.rs`:

```rust
    #[tokio::test]
    async fn a_thread_is_queued_once_however_often_it_is_offered() {
        // Turning the toggle on backfills; every completed run enqueues;
        // turning it off and on backfills again. One row per turn, always.
        let (core, _dir) = test_core().await;
        let account_id = crate::session::account_of(&core, crate::ids::TelegramId(4242))
            .await
            .unwrap();
        let turns = vec![
            scout_api::Turn { role: Role::You, text: "cheapest beans".to_string() },
            scout_api::Turn { role: Role::Scout, text: "here are three".to_string() },
        ];
        let queued = enqueue(&core, account_id, "4242", 1, &turns, false).await.unwrap();
        assert_eq!(queued, 2);
        let queued = enqueue(&core, account_id, "4242", 1, &turns, false).await.unwrap();
        assert_eq!(queued, 0, "the same thread was queued a second time");
        assert_eq!(pending(&core, 10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn what_telegram_already_showed_is_never_queued_for_telegram() {
        let (core, _dir) = test_core().await;
        let account_id = crate::session::account_of(&core, crate::ids::TelegramId(4242))
            .await
            .unwrap();
        let turns = vec![scout_api::Turn { role: Role::Scout, text: "here are three".to_string() }];
        enqueue(&core, account_id, "4242", 1, &turns, true).await.unwrap();
        assert!(pending(&core, 10).await.unwrap().is_empty());
        enqueue(&core, account_id, "4242", 1, &turns, false).await.unwrap();
        assert!(pending(&core, 10).await.unwrap().is_empty(), "the backfill echoed Telegram back at itself");
    }
```

`scout-core` has no shared `test_core` helper — `identity.rs` keeps a private one. Add the same four lines at the top of this `mod tests`:

```rust
    async fn test_core() -> (crate::core::Core, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mirror.duckdb");
        let cfg = crate::config::Config::for_test(path.to_str().unwrap());
        (crate::core::Core::start(cfg, None).unwrap(), dir)
    }
```

`tempfile` is already in this crate's dev-dependencies. Bind the `TempDir` — dropping it deletes the database underneath the test.

- [ ] **Step 3: Run it and watch it fail**

```bash
cargo test -p scout-core --lib mirror::
```

Expected: FAIL to compile, `cannot find function enqueue`.

- [ ] **Step 4: Write the implementation**

Add to `crates/scout-core/src/mirror.rs`, below `turn_key`:

```rust
use crate::core::Core;

pub use crate::store::PendingMirror;

/// Queues a thread's turns for one channel, skipping any already known.
/// Returns how many rows were written.
///
/// `delivered` is the channel saying "I have already shown the reader
/// these" — see the module's own tests, and the echo they exist to stop.
pub async fn enqueue(
    core: &Core,
    account_id: i64,
    address: &str,
    conversation_id: i64,
    turns: &[scout_api::Turn],
    delivered: bool,
) -> anyhow::Result<usize> {
    let store = core.store();
    let address = address.to_string();
    let rows: Vec<(String, String)> = turns
        .iter()
        .map(|t| (turn_key(conversation_id, t.role, &t.text), body_of(t)))
        .collect();
    let written = crate::core::blocking(move || {
        let mut written = 0;
        for (key, body) in &rows {
            if store.enqueue_mirror(account_id, TELEGRAM, &address, body, key, delivered)? {
                written += 1;
            }
        }
        Ok(written)
    })
    .await?;
    if written > 0 && !delivered {
        core.wake_mirror();
    }
    Ok(written)
}

/// How a turn reads once it is somebody else's message.
///
/// The reader's own question is quoted with a literal `>`, in plain text.
/// Not MarkdownV2: an answer is model output full of `*`, `_`, `[` and `.`,
/// every one of which MarkdownV2 requires escaped, and a missed escape turns
/// a price list into a parse error. A literal `>` reads fine and cannot fail.
fn body_of(turn: &scout_api::Turn) -> String {
    match turn.role {
        Role::You => turn.text.lines().map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n"),
        Role::Scout => turn.text.clone(),
    }
}

/// What is still waiting to go out, oldest first.
pub async fn pending(core: &Core, limit: usize) -> anyhow::Result<Vec<PendingMirror>> {
    let store = core.store();
    crate::core::blocking(move || store.pending_mirror(TELEGRAM, limit)).await
}

pub async fn sent(core: &Core, id: i64) -> anyhow::Result<()> {
    let store = core.store();
    crate::core::blocking(move || store.mark_mirror_sent(id)).await
}

pub async fn failed(core: &Core, id: i64) -> anyhow::Result<()> {
    let store = core.store();
    crate::core::blocking(move || store.mark_mirror_failed(id)).await
}

pub async fn is_enabled(core: &Core, account_id: i64) -> anyhow::Result<bool> {
    let store = core.store();
    crate::core::blocking(move || store.mirror_enabled(account_id)).await
}

pub async fn set_enabled(core: &Core, account_id: i64, on: bool) -> anyhow::Result<()> {
    let store = core.store();
    crate::core::blocking(move || store.set_mirror(account_id, on)).await
}
```

- [ ] **Step 5: Run it and watch it pass**

```bash
cargo test -p scout-core --lib mirror::
```

Expected: PASS, 5 tests.

- [ ] **Step 6: Add the quoting test**

```rust
    #[test]
    fn the_readers_own_words_are_quoted_line_by_line() {
        // A literal `>`, not a MarkdownV2 blockquote: the bot sends plain
        // text everywhere but two admin paths, and an unescaped `*` in an
        // answer would be a parse error rather than a price.
        let you = scout_api::Turn { role: Role::You, text: "find me\ntwo things".to_string() };
        assert_eq!(body_of(&you), "> find me\n> two things");
        let scout = scout_api::Turn { role: Role::Scout, text: "EUR 24.24 *delivered*".to_string() };
        assert_eq!(body_of(&scout), "EUR 24.24 *delivered*", "an answer must go out untouched");
    }
```

Run `cargo test -p scout-core --lib mirror::` — expected PASS, 6 tests.

- [ ] **Step 7: Commit**

```bash
git add crates/scout-core/src/mirror.rs crates/scout-core/src/core.rs
git commit -m "feat: queueing a thread, and waking whoever delivers it"
```

---

## Task 5: The toggle route

**Files:**
- Modify: `crates/scout-web/src/routes/chat.rs`

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/scout-web/src/routes/chat.rs`:

```rust
    #[tokio::test]
    async fn turning_the_mirror_on_queues_the_thread_that_is_already_there() {
        // The point of backfilling: you tick the box because you are about
        // to pick the thread up on your phone, and a thread that starts
        // mid-story is not the thread.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        seed_conversation(&core, account_id, "cheapest beans", "here are three").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);
        let res =
            post_json_with_cookie(&app, "/chat/mirror", &cookie, Some(&csrf), r#"{"on":true}"#).await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(scout_core::mirror::pending(&core, 10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn turning_it_on_twice_queues_the_thread_once() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        seed_conversation(&core, account_id, "cheapest beans", "here are three").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);
        for _ in 0..2 {
            post_json_with_cookie(&app, "/chat/mirror", &cookie, Some(&csrf), r#"{"on":true}"#).await;
        }
        assert_eq!(scout_core::mirror::pending(&core, 10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn the_mirror_cannot_be_switched_without_the_csrf_header() {
        // Same guard as /chat/messages and /chat/reset: without it, any page
        // on the internet can turn a reader's chat into a Telegram feed.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let res = post_json_with_cookie(&app, "/chat/mirror", &cookie, None, r#"{"on":true}"#).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(!scout_core::mirror::is_enabled(&core, account_id).await.unwrap());
    }
```

These are the helpers the neighbouring tests already use — `test_app_with_a_round`,
`admitted`, `session::mint`, `session::csrf_for`, `post_json_with_cookie` and
`seed_conversation`. Read `a_post_without_the_csrf_header_is_refused` in the same
file if any signature is unclear. Do not add new helpers.

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p scout-web --lib routes::chat::tests::turning_the_mirror_on
```

Expected: FAIL — the route 404s, so the status is `NOT_FOUND` rather than `NO_CONTENT`.

- [ ] **Step 3: Write the implementation**

Add the route in `router()` in `crates/scout-web/src/routes/chat.rs`, after `/chat/reset`:

```rust
        .route("/chat/mirror", post(mirror))
```

Add the handler beside `reset`:

```rust
#[derive(serde::Deserialize)]
struct MirrorIn {
    on: bool,
}

/// Switches mirroring for this account, and backfills the current thread
/// when switching on.
///
/// Backfilling here rather than in the drain because this is where the
/// decision is made, and because it is cheap: it writes rows and returns.
/// The drain does the sending, so a twenty-row backfill is a fast database
/// write and a slow background delivery, not a slow request.
///
/// Enqueueing is idempotent, so ticking the box twice — or after a spell
/// with it off — costs nothing and cannot duplicate a message.
async fn mirror(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<MirrorIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Err(e) = scout_core::mirror::set_enabled(&auth.core, account_id, body.on).await {
        tracing::error!(error = %e, "could not switch the mirror");
        return sorry();
    }
    if body.on {
        if let Err(e) = backfill(&auth, account_id).await {
            // The setting is saved either way. A backfill that failed is a
            // thread that starts late, which is worth logging and not worth
            // refusing the request over.
            tracing::warn!(error = %e, account_id, "could not queue the thread so far");
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Queues everything already said in the reader's current thread.
///
/// Silently does nothing when the account has no Telegram identity — the
/// toggle is not shown in that case, so reaching here means a hand-made
/// request, and there is nowhere to send to.
async fn backfill(auth: &AuthState, account_id: i64) -> anyhow::Result<()> {
    let Some(reply_to) = reply_to_for(auth, account_id).await else {
        return Ok(());
    };
    let Some((conversation_id, turns)) =
        scout_core::session::current_thread(&auth.core, account_id).await?
    else {
        return Ok(());
    };
    scout_core::mirror::enqueue(
        &auth.core,
        account_id,
        &reply_to.address,
        conversation_id,
        &turns,
        false,
    )
    .await?;
    Ok(())
}
```

`session::transcript` returns the turns but not the conversation id, and `turn_key` needs the id. Add this beside it in `crates/scout-core/src/session.rs`:

```rust
/// The current thread's id and transcript, or `None` when there is no
/// thread yet.
///
/// `transcript` answers the page, which only needs the turns. Mirroring
/// needs the id as well, because a turn's key is scoped to its conversation
/// — the same question asked again in a new thread is a new turn.
pub async fn current_thread(
    core: &Core,
    account_id: i64,
) -> anyhow::Result<Option<(i64, Vec<scout_api::Turn>)>> {
    let store = core.store();
    crate::core::blocking(move || {
        let Some(id) = latest_direct(&store, account_id)? else {
            return Ok(None);
        };
        Ok(Some((id, transcript_of(&store, id)?)))
    })
    .await
}
```

- [ ] **Step 4: Run them and watch them pass**

```bash
cargo test -p scout-web --lib routes::chat
```

Expected: PASS, including the three new tests.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-web/src/routes/chat.rs crates/scout-core/src/session.rs
git commit -m "feat: a switch that queues the thread you are already in"
```

---

## Task 6: The header control

**Files:**
- Modify: `crates/scout-web/src/chat.html`, `crates/scout-web/src/chat.js`, `crates/scout-web/src/routes/chat.rs`

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/scout-web/src/routes/chat.rs`:

```rust
    #[tokio::test]
    async fn the_mirror_toggle_is_absent_without_a_telegram_identity() {
        // A control that cannot work is a promise the page cannot keep. The
        // same call that decides whether a run may promise a reminder
        // decides whether this is shown, so the two cannot drift.
        let (app, core, _dir) = test_app_with_a_round().await;
        // `admitted` seeds a Telegram identity, so this account is built the
        // way `a_run_promises_a_reminder_only_where_one_could_be_delivered`
        // builds its web-only one. Copy that construction exactly.
        let account_id = web_only_account(&core).await;
        let page = get_chat(&app, account_id).await;
        assert!(!page.contains(r#"id="mirror""#), "offered a mirror with nowhere to send it");
    }

    #[tokio::test]
    async fn the_mirror_toggle_is_offered_to_someone_on_telegram() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let page = get_chat(&app, account_id).await;
        assert!(page.contains(r#"id="mirror""#));
    }
```

`a_run_promises_a_reminder_only_where_one_could_be_delivered` in the same file
already builds both a web-only account and a Telegram-linked one. Lift its
web-only construction into a `web_only_account(&core) -> i64` helper beside it
and call it from both places, rather than repeating it. Add a `get_chat`
helper that issues `GET /chat` with a session cookie and returns the body as a
`String`.

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p scout-web --lib routes::chat::tests::the_mirror_toggle
```

Expected: FAIL — `id="mirror"` is in neither page.

- [ ] **Step 3: Add the markup**

`/chat` is rendered from `chat.html`. Find where the page is served in `chat.rs` and how `#reset` reaches the markup; follow the same mechanism. In `crates/scout-web/src/chat.html`, inside `<header>`, immediately before the existing "New thread" button:

```html
  <button id="mirror" type="button" class="ghost" aria-pressed="false"
          title="Also send this thread to Telegram">
    <svg viewBox="0 0 24 24" width="17" height="17" aria-hidden="true">
      <path fill="currentColor" d="M21.7 3.4 2.9 10.6c-.9.3-.9 1.5.05 1.8l4.7 1.5 1.8 5.3c.3.8 1.3 1 1.9.4l2.6-2.5 4.6 3.4c.6.4 1.5.1 1.7-.7l3.2-15c.2-.9-.7-1.6-1.75-1.4z"/>
    </svg>
  </button>
```

Give it the same `.ghost` styling the "New thread" button already uses; if that button has no class, copy its declaration block into a `.ghost` rule and apply it to both rather than duplicating the CSS.

Add a rule so the on state is visible:

```css
  #mirror[aria-pressed="true"]{color:var(--blue); border-color:var(--blue)}
```

- [ ] **Step 4: Make it conditional**

The toggle must be removed from the page when there is no Telegram identity. In the `/chat` handler, after resolving `account_id`:

```rust
    // The same call that decides whether a run may promise a reminder
    // decides whether this is offered, so the two cannot disagree about
    // whether there is anywhere to send.
    let page = if reply_to_for(&auth, account_id).await.is_some() {
        TEMPLATE.to_string()
    } else {
        strip_mirror_toggle(TEMPLATE)
    };
```

with:

```rust
/// Removes the mirror control from the page.
///
/// A string edit rather than a template because this page is a static file
/// with one conditional element in it, and a templating engine for one
/// `if` is a dependency to maintain forever.
fn strip_mirror_toggle(page: &str) -> String {
    let Some(start) = page.find(r#"<button id="mirror""#) else {
        return page.to_string();
    };
    let Some(end) = page[start..].find("</button>") else {
        return page.to_string();
    };
    let mut out = String::with_capacity(page.len());
    out.push_str(&page[..start]);
    out.push_str(&page[start + end + "</button>".len()..]);
    out
}
```

The constant is `TEMPLATE` (`chat.rs:15`, `include_str!("../chat.html")`).

- [ ] **Step 5: Run them and watch them pass**

```bash
cargo test -p scout-web --lib routes::chat
```

Expected: PASS.

- [ ] **Step 6: Wire the click**

In `crates/scout-web/src/chat.js`, inside `start()`, beside the existing `#reset` handler:

```js
  const mirrorButton = document.getElementById('mirror')
  if (mirrorButton) {
    mirrorButton.addEventListener('click', async () => {
      // Read the state off the DOM rather than a variable: the button is
      // the only place it lives, and two copies would disagree the first
      // time a request failed.
      const on = mirrorButton.getAttribute('aria-pressed') !== 'true'
      mirrorButton.disabled = true
      try {
        const res = await fetch('/chat/mirror', {
          method: 'POST',
          headers: { 'content-type': 'application/json', 'x-scout-csrf': csrfToken },
          body: JSON.stringify({ on }),
        })
        if (!res.ok) throw new Error('refused')
        mirrorButton.setAttribute('aria-pressed', String(on))
        showNotice(on ? 'This thread is being sent to Telegram.' : 'No longer sending to Telegram.')
      } catch {
        showNotice('Could not change that. Try again.')
      } finally {
        mirrorButton.disabled = false
      }
    })
  }
```

- [ ] **Step 7: Check the page still has no inline script**

```bash
grep -c "<script>" crates/scout-web/src/chat.html
```

Expected: `0`. The CSP has no `'unsafe-inline'` for scripts, so an inline handler would be refused by the browser and by nothing else.

- [ ] **Step 8: Commit**

```bash
git add crates/scout-web/src/chat.html crates/scout-web/src/chat.js crates/scout-web/src/routes/chat.rs
git commit -m "feat: a switch in the header, and only where it can work"
```

---

## Task 7: Queue each completed turn

**Files:**
- Modify: `crates/scout-web/src/routes/chat.rs`

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn a_completed_turn_is_queued_only_when_the_mirror_is_on() {
        // `run_agent` needs a live model, so the enqueue is tested through
        // the function the run handler calls rather than through a run.
        let (_app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let auth = auth_state(&core);
        mirror_turn(&auth, account_id, 1, "cheapest beans", "here are three").await;
        assert!(
            scout_core::mirror::pending(&core, 10).await.unwrap().is_empty(),
            "queued a turn for someone who never asked for it"
        );
        scout_core::mirror::set_enabled(&core, account_id, true).await.unwrap();
        mirror_turn(&auth, account_id, 1, "cheapest beans", "here are three").await;
        assert_eq!(scout_core::mirror::pending(&core, 10).await.unwrap().len(), 2);
    }
```

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p scout-web --lib routes::chat::tests::a_completed_turn_is_queued
```

Expected: FAIL to compile, `cannot find function mirror_turn`.

- [ ] **Step 3: Write the implementation**

Add beside `backfill` in `crates/scout-web/src/routes/chat.rs`:

```rust
/// Queues one finished exchange, if the reader asked for that.
///
/// Called by the run handler rather than by `run_agent`, because
/// `run_agent` is shared by both channels and has no business knowing which
/// one it is serving. Telegram's own handler makes the opposite call, with
/// `delivered: true` — see the echo the two of them exist to prevent.
///
/// Every failure here is logged and swallowed. The answer is already on the
/// reader's screen and already saved to history; a mirror that did not get
/// queued is worth knowing about and is not worth failing a reply over.
async fn mirror_turn(auth: &AuthState, account_id: i64, conversation_id: i64, asked: &str, answered: &str) {
    match scout_core::mirror::is_enabled(&auth.core, account_id).await {
        Ok(false) => return,
        Ok(true) => {}
        Err(e) => {
            tracing::warn!(error = %e, "could not read the mirror setting");
            return;
        }
    }
    let Some(reply_to) = reply_to_for(auth, account_id).await else {
        return;
    };
    let turns = vec![
        scout_api::Turn { role: scout_api::Role::You, text: asked.to_string() },
        scout_api::Turn { role: scout_api::Role::Scout, text: answered.to_string() },
    ];
    if let Err(e) = scout_core::mirror::enqueue(
        &auth.core,
        account_id,
        &reply_to.address,
        conversation_id,
        &turns,
        false,
    )
    .await
    {
        tracing::warn!(error = %e, account_id, "could not queue a turn for Telegram");
    }
}
```

In `send_message`'s spawned task, the outcome is currently consumed straight into `end_frame`. Bind it first so the answer can be mirrored, and keep `end_frame` reading the same value:

```rust
        let outcome = scout_core::run::run_agent(&core, agent_tx, &run, &text).await;
        let _ = pump.await;
        // Mirror before the end frame so the queue is written whether or not
        // the reader's connection survived to see it.
        if let Ok(scout_core::run::RunOutcome::Answered(answer)) = &outcome {
            mirror_turn(&auth_for_mirror, account_id, conversation_id, &text, answer).await;
        }
        let _ = frames.send(Frame::End(end_frame(outcome)));
```

`auth` is moved into the task already in some form; clone whatever the task captures (`AuthState` is cheap to clone) and name the clone `auth_for_mirror` before the `tokio::spawn`.

- [ ] **Step 4: Run it and watch it pass**

```bash
cargo test -p scout-web --lib routes::chat
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-web/src/routes/chat.rs
git commit -m "feat: a finished exchange joins the queue"
```

---

## Task 8: Telegram records its own turns

**Files:**
- Modify: `crates/scout-telegram/src/bot.rs`

- [ ] **Step 1: Write the failing test**

`scout-telegram` has no way to build a `Core` in tests — it keeps no helper and
no `tempfile` dev-dependency, and every test in the crate is synchronous. The
*guarantee* is already covered on the core side by
`what_telegram_already_showed_is_never_queued_for_telegram` (Task 4). What is
left to protect here is that **both** handlers actually record, because a
missed one is silent: the backfill would echo that handler's messages back and
nothing would fail.

Add to `mod tests` in `crates/scout-telegram/src/bot.rs`:

```rust
    #[test]
    fn every_answer_this_bot_delivers_is_also_written_down() {
        // Two handlers bind `RunOutcome::Answered` — a message and a photo —
        // and a third arrived once before without anyone noticing. If one of
        // them delivers without recording, a browser backfill sends that
        // conversation back to the chat it came from, and nothing errors.
        //
        // Scoped to the source above the tests: this file contains the names
        // it is searching for, and two source-scan tests written today
        // matched their own prose and stayed green.
        let src = include_str!("bot.rs");
        let src = &src[..src.find("#[cfg(test)]").expect("the tests must come last")];
        assert_eq!(
            src.matches("RunOutcome::Answered(reply)").count(),
            src.matches("record_own_turn(").count() - 1,
            "an answered run is delivered without being recorded"
        );
    }
```

The `- 1` accounts for the function's own definition. If the counts disagree,
find the `RunOutcome::Answered` arm with no `record_own_turn` beside it.

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p scout-telegram every_answer_this_bot_delivers
```

Expected: FAIL — `record_own_turn` does not exist, so its count is 0 and the
subtraction underflows or the assertion fails.

- [ ] **Step 3: Write the implementation**

Add to `crates/scout-telegram/src/bot.rs`, beside `deliver`:

```rust
/// Records an exchange this chat has already seen, so a browser backfill
/// does not send it back.
///
/// Written unconditionally, not only when mirroring is on: the reader may
/// switch it on tomorrow, and by then the only way to know these messages
/// were already delivered is to have said so at the time.
async fn record_own_turn(
    core: &scout_core::core::Core,
    account_id: i64,
    chat_id: i64,
    conversation_id: i64,
    asked: &str,
    answered: &str,
) {
    let turns = vec![
        scout_api::Turn { role: scout_api::Role::You, text: asked.to_string() },
        scout_api::Turn { role: scout_api::Role::Scout, text: answered.to_string() },
    ];
    if let Err(e) = scout_core::mirror::enqueue(
        core,
        account_id,
        &chat_id.to_string(),
        conversation_id,
        &turns,
        true,
    )
    .await
    {
        tracing::warn!(error = %e, account_id, "could not record a Telegram turn");
    }
}
```

Call it from both places that bind `RunOutcome::Answered(reply)` — `bot.rs:959` and `bot.rs:1188` — immediately after `deliver` returns:

```rust
        Ok(scout_core::run::RunOutcome::Answered(reply)) => {
            deliver(&bot, &app, &mut live, chat_id, &reply).await?;
            record_own_turn(&app.core, account_id, chat_id.0, conversation_id, &prompt, &reply).await;
        }
```

Both call sites already have `account_id`, `conversation_id` and `prompt` in scope, from the `RunContext` built just above.

- [ ] **Step 4: Run it and watch it pass**

```bash
cargo test -p scout-telegram
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-telegram/src/bot.rs
git commit -m "feat: telegram writes down what it has already shown"
```

---

## Task 9: The drain

**Files:**
- Create: `crates/scout-telegram/src/mirror.rs`
- Modify: `crates/scout-telegram/src/main.rs`

`drain` takes the rows, a sink and a ledger — **not a `Core`.** Deciding what
to send and in what order needs no database, and `scout-telegram` has no way
to build a `Core` in a test. Passing the pieces in is what makes the rule
testable at all; `run` does the database work around it.

- [ ] **Step 1: Write the failing tests**

Create `crates/scout-telegram/src/mirror.rs`:

```rust
//! Sending what the browser queued.

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
```

`start_paused = true` because the drain paces itself; without it these tests
would sit through real seconds. `tokio`'s `test-util` feature is already in
this crate's dev-dependencies for exactly this reason.

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p scout-telegram mirror::
```

Expected: FAIL to compile, `cannot find trait Sink`.

- [ ] **Step 3: Write the implementation**

At the top of `crates/scout-telegram/src/mirror.rs`, below the module doc:

```rust
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
/// reads as nonsense. The stop is per account, so one reader who has blocked
/// the bot cannot freeze everybody else's thread behind them.
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
///
/// The notify is what makes it prompt; the tick is what makes a missed
/// signal a delay rather than a lost mirror.
pub async fn run(bot: Bot, core: Arc<Core>) {
    const TICK: Duration = Duration::from_secs(60);
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
```

Add `mod mirror;` to the module list at the top of `crates/scout-telegram/src/main.rs`, and spawn it beside the reminder scheduler:

```rust
    tokio::spawn(scheduler::run(telegram.clone(), core.clone()));
    tokio::spawn(mirror::run(telegram.clone(), core.clone()));
```

- [ ] **Step 4: Run them and watch them pass**

```bash
cargo test -p scout-telegram mirror::
```

Expected: PASS, 3 tests, finishing instantly.

- [ ] **Step 5: Mutation-check the ordering rule**

Replace `blocked.insert(row.account_id);` with `let _ = &blocked;` and run the
tests. `a_failure_stops_that_account_rather_than_racing_past_it` must fail.
Restore it.

- [ ] **Step 6: Commit**

```bash
git add crates/scout-telegram/src/mirror.rs crates/scout-telegram/src/main.rs
git commit -m "feat: send what the browser queued, in order"
```

---

## Task 10: The divider on a new thread

**Files:**
- Modify: `crates/scout-web/src/routes/chat.rs`

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn starting_a_new_thread_marks_the_seam_in_telegram() {
        // Without it, scrolling back through Telegram runs two unrelated
        // conversations together with nothing between them, which is
        // precisely the continuity this feature is for.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        seed_conversation(&core, account_id, "cheapest beans", "here are three").await;
        scout_core::mirror::set_enabled(&core, account_id, true).await.unwrap();
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);
        post_json_with_cookie(&app, "/chat/reset", &cookie, Some(&csrf), "{}").await;
        let queued = scout_core::mirror::pending(&core, 10).await.unwrap();
        assert!(
            queued.iter().any(|r| r.body.contains("New thread")),
            "no seam between two conversations"
        );
    }
```

`a_reset_starts_a_thread_that_does_not_remember_the_last_one` in the same file
already posts to `/chat/reset`; match its body and headers exactly if the empty
`{}` above does not suit the handler's extractor.

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p scout-web --lib routes::chat::tests::starting_a_new_thread_marks
```

Expected: FAIL, `no seam between two conversations`.

- [ ] **Step 3: Write the implementation**

In `reset` in `crates/scout-web/src/routes/chat.rs`, before starting the new conversation, capture the one being closed and queue a seam:

```rust
    // Queued before the new thread is started, because the divider belongs
    // to the conversation it closes -- that is what gives it a turn key
    // nobody else will mint, so pressing the button twice cannot send it
    // twice.
    if let Ok(true) = scout_core::mirror::is_enabled(&auth.core, account_id).await {
        if let Some(reply_to) = reply_to_for(&auth, account_id).await {
            if let Ok(Some((conversation_id, _))) =
                scout_core::session::current_thread(&auth.core, account_id).await
            {
                let seam = vec![scout_api::Turn {
                    role: scout_api::Role::Scout,
                    text: "— New thread —".to_string(),
                }];
                if let Err(e) = scout_core::mirror::enqueue(
                    &auth.core,
                    account_id,
                    &reply_to.address,
                    conversation_id,
                    &seam,
                    false,
                )
                .await
                {
                    tracing::warn!(error = %e, "could not queue a thread divider");
                }
            }
        }
    }
```

- [ ] **Step 4: Run it and watch it pass**

```bash
cargo test -p scout-web --lib routes::chat
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-web/src/routes/chat.rs
git commit -m "feat: a seam between two conversations"
```

---

## Task 11: Whole-workspace verification

- [ ] **Step 1: Every test, both runners**

```bash
cargo test --workspace 2>&1 | grep -E "^test result"
node --test 'crates/scout-web/src/*.test.mjs'
```

Expected: no failures. The Rust total should be 616 plus the tests added here; the JS total stays at 9.

- [ ] **Step 2: Clippy, with the right toolchain**

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo clippy --workspace --all-targets 2>&1 | grep -E "^warning|^error"
```

Expected: nothing but the pre-existing `proc-macro-error2` future-compatibility note.

- [ ] **Step 3: The CSP guard**

```bash
grep -c "<script>" crates/scout-web/src/chat.html
```

Expected: `0`.

- [ ] **Step 4: Confirm the schema version did not move**

```bash
cargo test -p scout-core --lib store::tests 2>&1 | grep -E "^test result"
```

Expected: PASS. The two new tables are in `MIGRATIONS`, which runs on every open, so `schema_version` stays at 6 and no numbered step was added.

- [ ] **Step 5: Commit anything outstanding**

```bash
git status --porcelain
```

Expected: clean.

---

## What this plan does not do

Stated so that nobody builds them by accident:

- **Telegram topics.** A thread started in the browser does not become a topic in Telegram. That needs the conversation model reworked first, and it carries live external risk — `teloxide-core` 0.13 predates private-chat topics, and [tdlib/telegram-bot-api#847](https://github.com/tdlib/telegram-bot-api/issues/847) reports outbound sends to private topics failing after the Bot API 10.0 rollout. Its own spec.
- **The web thread sidebar.** Same dependency.
- **Raising `HISTORY_CAP`.** Independent, and the number should come from measuring a real research thread.
- **Live mirroring.** The purpose is continuity, not watching a run from a phone.
- **Telegram → browser, live.** Reloading `/chat` already shows Telegram's turns.

The outbox is thread-agnostic, so none of the above forces it to be rebuilt: adding `message_thread_id` later is a column.
