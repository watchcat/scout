# Trip ↔ Chat Link Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A trip knows which chat created it, so deleting that chat deletes the trip, the trip view can message it, and legs can be added or removed without leaving the tab.

**Architecture:** One nullable column, `trips.conversation_id`. Explicit deletion cascades inside the existing transaction; the 48-hour idle sweep sets the column to `NULL` instead, which is what stops a timer from destroying travel plans. Two new account-scoped JSON routes edit legs, following the `choose_candidate_for_account` pattern of checking ownership and writing in one lock acquisition. The composer already on the page retargets, because Trips is a tab in the chat page rather than a separate one.

**Tech Stack:** Rust, axum 0.8, DuckDB (single writer behind a `Mutex`), vanilla ES modules, `node --test`.

---

## Read this first

**This repository is deliberately not rustfmt-formatted. Never run `cargo fmt`.** Match the hand-formatting of the file you are editing.

**Clippy needs the rustup toolchain.** A stale clippy from the nix store shadows it and fails with `E0514`. Always run:

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo clippy --workspace --all-targets
```

**A new column on an existing table needs two edits, not one.** `Store::open` runs `execute_batch(MIGRATIONS)` on every open, but `CREATE TABLE IF NOT EXISTS` does nothing to a table that already exists. So the column goes in *both* the `CREATE TABLE` (for fresh databases) and a numbered `ALTER TABLE` step (for existing ones). `steps()` currently ends at 8; the new step is 9.

**Column order matters.** `ALTER TABLE ... ADD COLUMN` appends. For a fresh database and a migrated one to have the same shape, the new column must be written **last** in the `CREATE TABLE` — after `updated_at`, before the `UNIQUE` constraint. The `conversations` table carries a comment explaining exactly this for its own step-7 columns; follow it.

**`Trip` deliberately hides its database id** (`#[serde(skip)] pub id`), which is why web routes address trips by *name*. Do not put the link on `Trip`. It goes on `Plan`, the channel-facing wrapper in `core/trips.rs`.

**Tests that pass for the wrong reason are the recurring failure in this codebase.** Several source-scanning tests here have matched the comment explaining a rule rather than the rule. When a test asserts on source text, scope it to the declaration and verify it goes red when the rule is removed.

---

## File structure

| File | Responsibility | Change |
|---|---|---|
| `crates/scout-core/src/store.rs` | schema, all SQL | column, `STEP_9`, `upsert_trip`, `delete_conversation`, `expire_conversations`, `remove_segment_checked`, `add_segment_for_account` |
| `crates/scout-core/src/tools/trips.rs` | the model's tools | carry `conversation_id`; make `iata`/`calendar_date` `pub(crate)` |
| `crates/scout-core/src/trips.rs` | channel-facing trip API | `Plan.chat`, `add_leg`, `remove_leg` |
| `crates/scout-core/src/agent.rs` | builds the tools | pass `conversation_id` |
| `crates/scout-web/src/routes/trips.rs` | JSON surface | two routes |
| `crates/scout-web/src/chat.js` | client | composer retarget, leg controls |
| `crates/scout-web/src/chat.html` | markup | composer target line, leg controls |

---

## Task 1: The column and the migration

**Files:**
- Modify: `crates/scout-core/src/store.rs` (trips `CREATE TABLE` ~line 90; `STEP_8_PINNED_NOT_NULL` ~line 743; `steps()` ~line 747)

- [ ] **Step 1: Write the failing test**

Add to `store.rs`'s `mod tests`:

```rust
    #[test]
    fn a_trip_carries_the_conversation_that_made_it() {
        // Nullable on purpose: NULL means orphaned, which is an ordinary
        // state a trip reaches by outliving its chat, not an error.
        let (store, _dir) = temp_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Atlantic loop", None, None).unwrap();
        let owner: Option<i64> = store
            .conn()
            .query_row("SELECT conversation_id FROM trips WHERE id = ?", params![trip.id], |r| r.get(0))
            .unwrap();
        assert_eq!(owner, None, "a trip made outside a conversation has no owner");
    }
```

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p scout-core a_trip_carries_the_conversation_that_made_it
```

Expected: FAIL — `Binder Error: Referenced column "conversation_id" not found`.

- [ ] **Step 3: Add the column in both places**

In `MIGRATIONS`, the `trips` table. The new column goes **last**, after `updated_at`:

```sql
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
    -- Last, so a fresh database and a migrated one — where this arrives by
    -- ALTER TABLE in step 9 — have the same column order. NULL means the
    -- chat that made this trip is gone; the next chat to touch it adopts it.
    conversation_id BIGINT,
    UNIQUE (account_id, name_key)
);
```

Then, immediately after `STEP_8_PINNED_NOT_NULL`:

```rust
const STEP_9_TRIP_CONVERSATION: &str = r#"
ALTER TABLE trips ADD COLUMN conversation_id BIGINT;
"#;
```

And register it in `steps()`:

```rust
        (8, Step::Sql(STEP_8_PINNED_NOT_NULL)),
        (9, Step::Sql(STEP_9_TRIP_CONVERSATION)),
```

- [ ] **Step 4: Run it and watch it pass**

```bash
cargo test -p scout-core a_trip_carries_the_conversation_that_made_it
```

Expected: PASS.

- [ ] **Step 5: Prove the migration path works, not just the fresh one**

The test above only exercises a database built by `CREATE TABLE`. Add:

```rust
    #[test]
    fn an_existing_database_gains_the_column_by_migration() {
        // The fresh-database path and the upgrade path are different code.
        // A CREATE TABLE IF NOT EXISTS does nothing to a table that already
        // exists, so without step 9 every deployed database would be missing
        // this column while every test passed.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scout.duckdb");
        {
            let conn = duckdb::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE SEQUENCE trips_id_seq;
                 CREATE TABLE trips (
                     id BIGINT PRIMARY KEY DEFAULT nextval('trips_id_seq'),
                     account_id BIGINT NOT NULL, name TEXT NOT NULL,
                     name_key TEXT NOT NULL, adults BIGINT NOT NULL DEFAULT 1,
                     cabin_class TEXT, status TEXT NOT NULL DEFAULT 'planning',
                     created_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
                     updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
                     UNIQUE (account_id, name_key));
                 CREATE TABLE schema_version (version BIGINT NOT NULL);
                 INSERT INTO schema_version VALUES (8);",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let n: i64 = store
            .conn()
            .query_row(
                "SELECT count(*) FROM information_schema.columns
                 WHERE table_name = 'trips' AND column_name = 'conversation_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "step 9 must add the column to a database that predates it");
    }
```

- [ ] **Step 6: Run both**

```bash
cargo test -p scout-core conversation_that_made_it
cargo test -p scout-core an_existing_database_gains_the_column
```

Expected: both PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/scout-core/src/store.rs
git commit -m "feat: trips carry the conversation that made them"
```

---

## Task 2: Creation sets the owner; adoption only fills a hole

**Files:**
- Modify: `crates/scout-core/src/store.rs` — `upsert_trip` (~line 2427)

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn the_chat_that_made_a_trip_keeps_it_and_an_orphan_is_adopted() {
        // Ownership is single because the composer needs exactly one place
        // to send to. A live owner is never displaced: otherwise deleting
        // your most recent chat would destroy a trip whose original
        // planning thread is still sitting there.
        let (store, _dir) = temp_store();
        let account = store.account_for_telegram(1).unwrap();

        let trip = store.upsert_trip(account, "Atlantic loop", None, None, Some(11)).unwrap();
        assert_eq!(store.trip_owner(trip.id).unwrap(), Some(11));

        // A second chat extending the same trip must NOT take it.
        store.upsert_trip(account, "atlantic loop", None, None, Some(22)).unwrap();
        assert_eq!(store.trip_owner(trip.id).unwrap(), Some(11), "a live owner is never displaced");

        // Orphaned, then touched again: adopted.
        store.detach_trips_of(&[11]).unwrap();
        assert_eq!(store.trip_owner(trip.id).unwrap(), None);
        store.upsert_trip(account, "Atlantic loop", None, None, Some(33)).unwrap();
        assert_eq!(store.trip_owner(trip.id).unwrap(), Some(33), "an orphan is adopted");
    }

    #[test]
    fn a_trip_made_with_no_conversation_stays_unowned() {
        // Telegram group flows and tests both create trips without a
        // conversation. That must be a plain None, not a panic or a zero.
        let (store, _dir) = temp_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Japan in spring", None, None, None).unwrap();
        assert_eq!(store.trip_owner(trip.id).unwrap(), None);
    }
```

- [ ] **Step 2: Run and watch it fail**

```bash
cargo test -p scout-core the_chat_that_made_a_trip_keeps_it
```

Expected: FAIL to compile — `upsert_trip` takes 4 arguments, `trip_owner` and `detach_trips_of` do not exist.

- [ ] **Step 3: Add the two helpers**

In `impl Store`, beside the other trip methods:

```rust
    /// Which conversation owns this trip, if any. Test and diagnostic
    /// support: production reads it through `Plan`.
    pub fn trip_owner(&self, trip_id: i64) -> Result<Option<i64>> {
        let conn = self.conn();
        Ok(conn.query_row(
            "SELECT conversation_id FROM trips WHERE id = ?",
            params![trip_id],
            |row| row.get(0),
        )?)
    }

    /// Releases the trips owned by these conversations without touching the
    /// trips themselves.
    ///
    /// This is what expiry uses, and the distinction it draws is the whole
    /// point of the feature: a thread that a *timer* removed must not take a
    /// travel plan with it. Only `delete_conversation` — someone pressed
    /// Delete — cascades.
    pub fn detach_trips_of(&self, conversation_ids: &[i64]) -> Result<usize> {
        if conversation_ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn();
        Ok(detach_trips_within(&conn, conversation_ids)?)
    }
```

And a free function beside `load_trip`, so the transactional callers can reuse it without re-locking:

```rust
/// `detach_trips_of`'s body, taking a connection so a caller already inside
/// a transaction can use it.
fn detach_trips_within(conn: &Connection, conversation_ids: &[i64]) -> Result<usize> {
    let holes = ["?"].repeat(conversation_ids.len()).join(", ");
    Ok(conn.execute(
        &format!("UPDATE trips SET conversation_id = NULL WHERE conversation_id IN ({holes})"),
        duckdb::params_from_iter(conversation_ids.iter()),
    )?)
}
```

- [ ] **Step 4: Widen `upsert_trip`**

Change the signature and add the ownership write. The new parameter is last:

```rust
    pub fn upsert_trip(
        &self,
        account_id: i64,
        name: &str,
        adults: Option<i64>,
        cabin_class: Option<&str>,
        conversation_id: Option<i64>,
    ) -> Result<Trip> {
```

Immediately after the existing `INSERT ... ON CONFLICT DO NOTHING`, add:

```rust
        // Only ever fills a hole. `IS NULL` is what makes the creator keep
        // the trip while an orphan gets adopted, in one statement and with
        // no second code path.
        if let Some(conversation_id) = conversation_id {
            conn.execute(
                "UPDATE trips SET conversation_id = ?
                 WHERE account_id = ? AND name_key = ? AND conversation_id IS NULL",
                params![conversation_id, account_id, key],
            )?;
        }
```

- [ ] **Step 5: Fix every caller**

There are two in production and several in tests. Pass `None` at both production sites for now — Task 5 threads the real value:

```bash
grep -rn "upsert_trip(" --include="*.rs" crates/
```

`crates/scout-core/src/trips.rs:85` and `crates/scout-core/src/tools/trips.rs:820` each gain a trailing `None,` argument. Test callers likewise.

- [ ] **Step 6: Run the tests**

```bash
cargo test -p scout-core trip
```

Expected: PASS, including the two new ones.

- [ ] **Step 7: Mutation-check the guard**

Temporarily delete ` AND conversation_id IS NULL` from the `UPDATE` in step 4 and re-run:

```bash
cargo test -p scout-core the_chat_that_made_a_trip_keeps_it
```

Expected: FAIL on "a live owner is never displaced". Restore the clause and confirm it passes again. If it stayed green, the test is not testing what it claims and must be fixed before proceeding.

- [ ] **Step 8: Commit**

```bash
git add crates/scout-core/src/store.rs crates/scout-core/src/trips.rs crates/scout-core/src/tools/trips.rs
git commit -m "feat: the chat that makes a trip owns it, and an orphan is adopted"
```

---

## Task 3: Explicit delete cascades

**Files:**
- Modify: `crates/scout-core/src/store.rs` — `delete_conversation` (~line 1437)

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn deleting_a_thread_deletes_the_trip_it_owns_and_nothing_else() {
        // Pressing Delete is a decision, so it takes the plan with it. The
        // trip owned by another thread is the control: a cascade that is
        // too wide is worse than none.
        let (store, _dir) = temp_store();
        let account = store.account_for_telegram(1).unwrap();
        let doomed = store.start_conversation(account, "direct").unwrap();
        let spared = store.start_conversation(account, "direct").unwrap();

        let a = store.upsert_trip(account, "Atlantic loop", None, None, Some(doomed)).unwrap();
        let b = store.upsert_trip(account, "Japan in spring", None, None, Some(spared)).unwrap();

        assert!(store.delete_conversation(account, doomed).unwrap());

        assert!(store.find_trip(account, "Atlantic loop").unwrap().is_none(), "its trip goes with it");
        assert!(store.find_trip(account, "Japan in spring").unwrap().is_some(), "another thread's trip stays");
        // The segments of the deleted trip must not survive it.
        let orphans: i64 = store
            .conn()
            .query_row("SELECT count(*) FROM trip_segments WHERE trip_id = ?", params![a.id], |r| r.get(0))
            .unwrap();
        assert_eq!(orphans, 0, "a deleted trip leaves no segments behind");
        let _ = b;
    }
```

- [ ] **Step 2: Run and watch it fail**

```bash
cargo test -p scout-core deleting_a_thread_deletes_the_trip
```

Expected: FAIL on "its trip goes with it" — the trip is still there.

- [ ] **Step 3: Cascade inside the existing transaction**

In `delete_conversation`, inside the closure, after the `DELETE FROM messages`:

```rust
            conn.execute("DELETE FROM messages WHERE conversation_id = ?", params![conversation_id])?;
            // The trips this thread owns go with it. Inside this same
            // transaction: a cascade that can half-happen would leave a trip
            // pointing at a conversation that no longer exists.
            //
            // Only here. `expire_conversations` detaches instead — see
            // `detach_trips_within`. That difference is the feature.
            conn.execute(
                "DELETE FROM segment_candidates WHERE trip_id IN
                     (SELECT id FROM trips WHERE conversation_id = ?)",
                params![conversation_id],
            )?;
            conn.execute(
                "DELETE FROM trip_segments WHERE trip_id IN
                     (SELECT id FROM trips WHERE conversation_id = ?)",
                params![conversation_id],
            )?;
            conn.execute(
                "DELETE FROM trips WHERE conversation_id = ?",
                params![conversation_id],
            )?;
            Ok(true)
```

- [ ] **Step 4: Run it**

```bash
cargo test -p scout-core deleting_a_thread_deletes_the_trip
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/store.rs
git commit -m "feat: deleting a thread deletes the trips it owns"
```

---

## Task 4: The sweep detaches — the load-bearing task

**Files:**
- Modify: `crates/scout-core/src/store.rs` — `expire_conversations` (~line 1488)

This is the task the whole design exists for. Threads expire after 48 idle hours. If expiry cascaded, every trip would vanish two days after its chat went quiet, with no button pressed and nothing to undo.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn an_expired_thread_releases_its_trip_instead_of_destroying_it() {
        // The single most important test in this feature. Threads expire on
        // a 48-hour timer; trips are built over weeks. If expiry ever
        // cascades the way `delete_conversation` does, every travel plan
        // disappears two days after its chat goes quiet — silently, with
        // nothing to undo. This test is what stands in the way.
        let (store, _dir) = temp_store();
        let account = store.account_for_telegram(1).unwrap();
        let stale = store.start_conversation(account, "direct").unwrap();
        let trip = store.upsert_trip(account, "Japan in spring", None, None, Some(stale)).unwrap();

        store
            .conn()
            .execute(
                "UPDATE conversations SET updated_at = current_timestamp - to_seconds(?) WHERE id = ?",
                params![60 * 60 * 72, stale],
            )
            .unwrap();

        let gone = store.expire_conversations(48 * 3600, &[]).unwrap();
        assert_eq!(gone, 1, "the stale thread expired");

        let kept = store.find_trip(account, "Japan in spring").unwrap();
        assert!(kept.is_some(), "a timer must never destroy a travel plan");
        assert_eq!(store.trip_owner(trip.id).unwrap(), None, "and the dead link is released");
    }
```

- [ ] **Step 2: Run and watch it fail**

```bash
cargo test -p scout-core an_expired_thread_releases_its_trip
```

Expected: FAIL on the owner assertion — the trip survives (nothing deletes it yet) but still points at a conversation that is gone. That dangling id is the bug this task closes.

- [ ] **Step 3: Detach before deleting**

In `expire_conversations`, inside the transaction closure and **before** the `DELETE FROM conversations`, collect the doomed ids and release their trips:

```rust
            // Which threads are about to go. Read before the DELETE, because
            // afterwards there is nothing left to join against.
            let mut stmt = conn.prepare(&format!(
                "SELECT id FROM conversations
                 WHERE NOT pinned
                   AND updated_at < CAST(current_timestamp AS TIMESTAMP) - to_seconds(?){not_running}"
            ))?;
            let doomed: Vec<i64> = stmt
                .query_map(duckdb::params_from_iter(args.iter()), |row| row.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            // Released, not deleted. `delete_conversation` cascades because
            // somebody pressed Delete; this is a timer, and a timer must not
            // destroy a plan the traveller is still building.
            //
            // `doomed` is empty on every sweep that expires nothing, which is
            // the normal hourly case. `detach_trips_within` guards that —
            // an `IN ()` is a parser error, not an empty set.
            detach_trips_within(&conn, &doomed)?;
```

- [ ] **Step 4: Run it**

```bash
cargo test -p scout-core an_expired_thread_releases_its_trip
```

Expected: PASS.

- [ ] **Step 5: Mutation-check — prove the test would catch a cascade**

Temporarily replace the `detach_trips_within(&conn, &doomed)?;` line with a cascade:

```rust
            conn.execute("DELETE FROM trips WHERE conversation_id IN (SELECT id FROM conversations WHERE NOT pinned)", [])?;
```

Re-run:

```bash
cargo test -p scout-core an_expired_thread_releases_its_trip
```

Expected: FAIL on "a timer must never destroy a travel plan". **Restore the correct line.** If the test stayed green, it does not protect what it claims to and must be fixed before going further.

- [ ] **Step 6: Commit**

```bash
git add crates/scout-core/src/store.rs
git commit -m "fix: an expired thread releases its trip rather than destroying it"
```

---

## Task 5: Thread the conversation id through the tools

**Files:**
- Modify: `crates/scout-core/src/tools/trips.rs` (structs at ~515, ~770, ~919 and the others that write)
- Modify: `crates/scout-core/src/agent.rs` (where the tools are constructed)

- [ ] **Step 1: Find every writing tool and its construction site**

```bash
grep -n "pub account_id: i64," crates/scout-core/src/tools/trips.rs
grep -rn "AddTripSegmentTool\|AddTripOptionTool\|FinaliseTripTool\|UpdateTripSegmentTool\|DropTripSegmentTool" --include="*.rs" crates/scout-core/src/agent.rs
```

- [ ] **Step 2: Add the field to each writing tool**

For every tool struct in `tools/trips.rs` that *writes* (creates a trip, adds a segment, adds or chooses an option, finalises), add beside `pub account_id: i64,`:

```rust
    /// The conversation this run belongs to, so a trip it creates knows
    /// which chat to die with. `None` where a channel has no conversation.
    pub conversation_id: Option<i64>,
```

Read-only tools (`ShowTripTool`) do not get it.

- [ ] **Step 3: Pass it at the one call that creates a trip**

In `AddTripSegmentTool::call` (~line 818), the `upsert_trip` call becomes:

```rust
        let store = self.store.clone();
        let account_id = self.account_id;
        let conversation_id = self.conversation_id;
        tokio::task::spawn_blocking(move || -> anyhow::Result<Trip> {
            let trip = store.upsert_trip(
                account_id,
                &args.trip,
                args.adults,
                args.cabin_class.as_deref(),
                conversation_id,
            )?;
```

- [ ] **Step 4: Pass it where the tools are built**

In `agent.rs`, every construction of a writing trip tool gains `conversation_id,` beside `account_id`. The value is already in scope wherever the run's conversation is known; if a construction site does not have it, thread it from the same place `account_id` arrives.

- [ ] **Step 5: Build and run the suite**

```bash
cargo build --workspace 2>&1 | grep -E "^error" | head
cargo test --workspace 2>&1 | grep -E "^test result|^error"
```

Expected: compiles, all green.

- [ ] **Step 6: Write the test that this actually reaches the database**

In `tools/trips.rs`'s `mod tests`:

```rust
    #[tokio::test]
    async fn a_trip_the_model_creates_belongs_to_the_chat_it_was_asked_in() {
        // The field existing is not the same as the field arriving. Without
        // this, every production trip would be created with None and the
        // whole feature would be dead while every other test passed.
        let (store, _dir) = temp_store();
        let account = store.account_for_telegram(1).unwrap();
        let conversation = store.start_conversation(account, "direct").unwrap();
        let tool = AddTripSegmentTool {
            store: store.clone(),
            account_id: account,
            conversation_id: Some(conversation),
        };
        tool.call(AddSegmentArgs {
            trip: "Atlantic loop".to_string(),
            position: None,
            origin: "AMS".to_string(),
            destination: "LIS".to_string(),
            departure_date: "2026-10-12".to_string(),
            adults: None,
            cabin_class: None,
        })
        .await
        .unwrap();
        let trip = store.find_trip(account, "Atlantic loop").unwrap().unwrap();
        assert_eq!(store.trip_owner(trip.id).unwrap(), Some(conversation));
    }
```

Adjust the `AddSegmentArgs` literal to match the struct's actual fields (`grep -nA12 "pub struct AddSegmentArgs" crates/scout-core/src/tools/trips.rs`) and `AddTripSegmentTool`'s actual fields — do not guess.

- [ ] **Step 7: Run it**

```bash
cargo test -p scout-core a_trip_the_model_creates_belongs_to_the_chat
```

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/scout-core/src/tools/trips.rs crates/scout-core/src/agent.rs
git commit -m "feat: a trip the model creates belongs to the chat it was asked in"
```

---

## Task 6: `Plan` carries the chat, so the client can name it

**Files:**
- Modify: `crates/scout-core/src/store.rs` — a read for the owning thread
- Modify: `crates/scout-core/src/trips.rs` — `Plan`

The link goes on `Plan`, not `Trip`: `Trip` is the model-facing type and hides its own id on purpose.

- [ ] **Step 1: Write the failing test**

In `trips.rs`'s `mod tests`:

```rust
    #[tokio::test]
    async fn a_plan_names_the_chat_it_belongs_to() {
        // The composer has to say where a message will land. An orphan says
        // so by carrying None, which the client renders as "a new chat".
        let (core, _dir) = temp_core().await;
        let account = core.store().account_for_telegram(1).unwrap();
        let conversation = core.store().start_conversation(account, "direct").unwrap();
        core.store().set_thread_title(account, conversation, "Cheap flights in October").unwrap();
        core.store().upsert_trip(account, "Atlantic loop", None, None, Some(conversation)).unwrap();

        let plans = list(&core, account).await.unwrap();
        let chat = plans[0].chat.as_ref().expect("an owned trip names its chat");
        assert_eq!(chat.id, conversation);
        assert_eq!(chat.title.as_deref(), Some("Cheap flights in October"));
        assert_eq!(chat.scope, "direct");
    }
```

Check the real title-setting method name before writing this:

```bash
grep -n "pub fn set_thread_title\|pub fn rename_thread\|title" crates/scout-core/src/store.rs | grep "pub fn"
```

Use whatever it is actually called.

- [ ] **Step 2: Run and watch it fail**

```bash
cargo test -p scout-core a_plan_names_the_chat_it_belongs_to
```

Expected: FAIL to compile — no field `chat` on `Plan`.

- [ ] **Step 3: Add the type and the read**

In `store.rs`:

```rust
/// The conversation a trip belongs to, as much of it as a client needs to
/// name the place a message will land.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TripChat {
    pub id: i64,
    pub title: Option<String>,
    /// `direct` is the thread web and 1:1 Telegram share. Anything else is
    /// a room the web client must not post into.
    pub scope: String,
}

impl Store {
    /// The conversation that owns this trip, if it still exists.
    pub fn trip_chat(&self, trip_id: i64) -> Result<Option<TripChat>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT c.id, c.title, c.scope FROM conversations c
             JOIN trips t ON t.conversation_id = c.id
             WHERE t.id = ?",
        )?;
        let found = stmt
            .query_map(params![trip_id], |row| {
                Ok(TripChat { id: row.get(0)?, title: row.get(1)?, scope: row.get(2)? })
            })?
            .next()
            .transpose()?;
        Ok(found)
    }
}
```

- [ ] **Step 4: Put it on `Plan`**

In `trips.rs`:

```rust
pub struct Plan {
    #[serde(flatten)]
    pub trip: Trip,
    pub not_ready: Option<String>,
    pub notes: Vec<String>,
    /// The chat this trip belongs to. `None` is orphaned — an ordinary
    /// state, reached by outliving the chat that made it.
    pub chat: Option<crate::store::TripChat>,
}
```

`Plan::from_trip` cannot read the store, so give it the chat:

```rust
impl Plan {
    fn from_trip(trip: Trip, chat: Option<crate::store::TripChat>) -> Self {
        let not_ready = crate::tools::trips::ready_to_price(&trip.segments)
            .err()
            .or_else(|| crate::tools::trips::dates_run_forwards(&trip.segments).err());
        let notes = crate::tools::trips::itinerary_notes(&trip.segments);
        Self { trip, not_ready, notes, chat }
    }
}
```

And in `list`, resolve it inside the same blocking closure:

```rust
pub async fn list(core: &Core, account_id: i64) -> anyhow::Result<Vec<Plan>> {
    let store = core.store();
    blocking(move || {
        let trips = store.list_trips(account_id)?;
        trips
            .into_iter()
            .map(|trip| {
                let chat = store.trip_chat(trip.id)?;
                Ok(Plan::from_trip(trip, chat))
            })
            .collect()
    })
    .await
}
```

Fix the other `Plan::from_trip` call sites the compiler points at, passing the chat where one is known and `None` otherwise.

- [ ] **Step 5: Run it**

```bash
cargo test -p scout-core a_plan_names_the_chat_it_belongs_to
cargo test -p scout-core trips
```

Expected: both PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/scout-core/src/store.rs crates/scout-core/src/trips.rs
git commit -m "feat: a plan names the chat it belongs to"
```

---

## Task 7: Adding and removing a leg, in core

**Files:**
- Modify: `crates/scout-core/src/tools/trips.rs` — `iata`, `calendar_date` visibility
- Modify: `crates/scout-core/src/store.rs` — `remove_segment_checked`
- Modify: `crates/scout-core/src/trips.rs` — `add_leg`, `remove_leg`

- [ ] **Step 1: Write the failing test for the stale-position guard**

In `store.rs`'s `mod tests`:

```rust
    #[test]
    fn a_stale_remove_refuses_rather_than_deleting_the_wrong_leg() {
        // `drop_segment` renumbers: removing position 0 shifts position 1
        // down to 0. A browser tab holding a trip drawn thirty seconds ago
        // is therefore one concurrent edit away from asking to delete
        // "leg 1" and destroying a leg that is no longer the one it drew.
        // `add_candidate` already guards this way and says why; this is the
        // same guard on the same hazard.
        let (store, _dir) = temp_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Atlantic loop", None, None, None).unwrap();
        store.add_segment(trip.id, None, "AMS", "LIS", "2026-10-12").unwrap();
        store.add_segment(trip.id, None, "LIS", "FCO", "2026-10-14").unwrap();

        // The browser drew both legs, then someone removed the first.
        store.drop_segment(trip.id, 0).unwrap();

        // The stale click: "remove leg 1", which the browser believes is
        // LIS→FCO. After the renumber, position 1 does not exist and
        // position 0 IS LIS→FCO.
        let stale = ExpectedSegment {
            origin: "LIS",
            destination: "FCO",
            departure_date: Some("2026-10-14"),
        };
        let refused = store.remove_segment_checked(trip.id, 1, stale).unwrap();
        assert!(!refused, "a position that no longer exists must refuse");

        let after = store.find_trip(account, "Atlantic loop").unwrap().unwrap();
        assert_eq!(after.segments.len(), 1, "the surviving leg is untouched");
        assert_eq!(after.segments[0].destination, "FCO");
    }

    #[test]
    fn a_remove_that_matches_what_the_reader_saw_goes_through() {
        let (store, _dir) = temp_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Atlantic loop", None, None, None).unwrap();
        store.add_segment(trip.id, None, "AMS", "LIS", "2026-10-12").unwrap();
        store.add_segment(trip.id, None, "LIS", "FCO", "2026-10-14").unwrap();

        let seen = ExpectedSegment {
            origin: "LIS",
            destination: "FCO",
            departure_date: Some("2026-10-14"),
        };
        assert!(store.remove_segment_checked(trip.id, 1, seen).unwrap());
        let after = store.find_trip(account, "Atlantic loop").unwrap().unwrap();
        assert_eq!(after.segments.len(), 1);
        assert_eq!(after.segments[0].destination, "LIS");
    }
```

- [ ] **Step 2: Run and watch them fail**

```bash
cargo test -p scout-core remove_segment_checked
```

Expected: FAIL to compile — no method `remove_segment_checked`.

- [ ] **Step 3: Implement the guarded remove**

Beside `drop_segment` in `store.rs`:

```rust
    /// `drop_segment`, but only if the segment is still what the caller saw.
    ///
    /// Returns `false` when it is not — a stale browser tab, which is an
    /// ordinary race and not a fault. The check and the delete are one lock
    /// acquisition on purpose: a caller that read the trip, decided, and
    /// then called `drop_segment` would be checking against a snapshot that
    /// a concurrent renumber can invalidate in between. Same reasoning as
    /// `add_candidate`'s `expected`.
    pub fn remove_segment_checked(
        &self,
        trip_id: i64,
        position: i64,
        expected: ExpectedSegment<'_>,
    ) -> Result<bool> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT origin, destination, departure_date FROM trip_segments
             WHERE trip_id = ? AND position = ?",
        )?;
        let found: Option<(String, String, String)> = stmt
            .query_map(params![trip_id, position], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .next()
            .transpose()?;
        drop(stmt);
        let Some((origin, destination, date)) = found else {
            return Ok(false);
        };
        if origin != expected.origin || destination != expected.destination {
            return Ok(false);
        }
        if expected.departure_date.is_some_and(|d| d != date) {
            return Ok(false);
        }
        drop(conn);
        self.drop_segment(trip_id, position)?;
        Ok(true)
    }
```

Note: `drop_segment` takes its own `self.conn()`, so the guard drops the guard-read's connection first. If `conn()` is a re-entrant `MutexGuard` this will deadlock — check how `conn()` is defined (`grep -n "fn conn" crates/scout-core/src/store.rs`) and if so, extract `drop_segment`'s body into a `fn drop_segment_within(conn: &Connection, ...)` free function and call that instead, keeping one acquisition throughout.

- [ ] **Step 4: Run the two tests**

```bash
cargo test -p scout-core remove_segment_checked
cargo test -p scout-core a_remove_that_matches_what_the_reader_saw
```

Expected: both PASS.

- [ ] **Step 5: Make the validators reusable**

In `tools/trips.rs`, change both to `pub(crate)`:

```rust
pub(crate) fn iata(label: &str, value: &str) -> Result<String, StoreToolError> {
pub(crate) fn calendar_date(value: &str) -> Result<String, StoreToolError> {
```

- [ ] **Step 6: Add the channel-facing operations**

In `trips.rs`, beside `choose`:

```rust
/// What a leg edit did. A trip that vanished and a leg that moved are
/// ordinary races in a browser tab, not internal errors.
#[derive(Debug, Clone, PartialEq)]
pub enum LegEdit {
    Done(Plan),
    TripNotFound,
    SegmentChanged,
    Invalid(String),
}

/// Adds a leg to a trip the account owns.
pub async fn add_leg(
    core: &Core,
    account_id: i64,
    trip_name: &str,
    position: Option<i64>,
    origin: &str,
    destination: &str,
    departure_date: &str,
) -> anyhow::Result<LegEdit> {
    // The same validators the model's tools use, so the web and the model
    // cannot disagree about what a valid leg is.
    let origin = match crate::tools::trips::iata("origin", origin) {
        Ok(code) => code,
        Err(e) => return Ok(LegEdit::Invalid(e.to_string())),
    };
    let destination = match crate::tools::trips::iata("destination", destination) {
        Ok(code) => code,
        Err(e) => return Ok(LegEdit::Invalid(e.to_string())),
    };
    if origin == destination {
        return Ok(LegEdit::Invalid(format!(
            "origin and destination are both {origin}; a flight needs two different places"
        )));
    }
    let date = match crate::tools::trips::calendar_date(departure_date) {
        Ok(d) => d,
        Err(e) => return Ok(LegEdit::Invalid(e.to_string())),
    };

    let store = core.store();
    let name = trip_name.to_string();
    blocking(move || {
        let Some(trip) = store.find_trip(account_id, &name)? else {
            return Ok(LegEdit::TripNotFound);
        };
        store.add_segment(trip.id, position, &origin, &destination, &date)?;
        let updated = store.find_trip(account_id, &name)?;
        Ok(match updated {
            Some(t) => {
                let chat = store.trip_chat(t.id)?;
                LegEdit::Done(Plan::from_trip(t, chat))
            }
            None => LegEdit::TripNotFound,
        })
    })
    .await
}

/// Removes a leg, but only if it is still the leg the reader saw.
pub async fn remove_leg(
    core: &Core,
    account_id: i64,
    trip_name: &str,
    position: i64,
    origin: &str,
    destination: &str,
    departure_date: Option<&str>,
) -> anyhow::Result<LegEdit> {
    let store = core.store();
    let name = trip_name.to_string();
    let (origin, destination) = (origin.to_string(), destination.to_string());
    let date = departure_date.map(|d| d.to_string());
    blocking(move || {
        let Some(trip) = store.find_trip(account_id, &name)? else {
            return Ok(LegEdit::TripNotFound);
        };
        let expected = crate::store::ExpectedSegment {
            origin: &origin,
            destination: &destination,
            departure_date: date.as_deref(),
        };
        if !store.remove_segment_checked(trip.id, position, expected)? {
            return Ok(LegEdit::SegmentChanged);
        }
        let updated = store.find_trip(account_id, &name)?;
        Ok(match updated {
            Some(t) => {
                let chat = store.trip_chat(t.id)?;
                LegEdit::Done(Plan::from_trip(t, chat))
            }
            None => LegEdit::TripNotFound,
        })
    })
    .await
}
```

`Plan::from_trip` is private to the module; both callers are inside it, so no visibility change is needed.

- [ ] **Step 7: Test the account scoping**

```rust
    #[tokio::test]
    async fn one_account_cannot_edit_anothers_trip() {
        // Trips are addressed by name, and two accounts can both have a
        // trip called "Atlantic loop". Scoping is what stops one traveller
        // deleting a leg from the other's plan.
        let (core, _dir) = temp_core().await;
        let mine = core.store().account_for_telegram(1).unwrap();
        let theirs = core.store().account_for_telegram(2).unwrap();
        let trip = core.store().upsert_trip(theirs, "Atlantic loop", None, None, None).unwrap();
        core.store().add_segment(trip.id, None, "AMS", "LIS", "2026-10-12").unwrap();

        let out = remove_leg(&core, mine, "Atlantic loop", 0, "AMS", "LIS", Some("2026-10-12"))
            .await
            .unwrap();
        assert_eq!(out, LegEdit::TripNotFound, "another account's trip is not found, not edited");

        let untouched = core.store().find_trip(theirs, "Atlantic loop").unwrap().unwrap();
        assert_eq!(untouched.segments.len(), 1);
    }
```

- [ ] **Step 8: Run**

```bash
cargo test -p scout-core leg
```

Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/scout-core/src/store.rs crates/scout-core/src/trips.rs crates/scout-core/src/tools/trips.rs
git commit -m "feat: add and remove a leg, with a guard against a stale position"
```

---

## Task 8: The web routes

**Files:**
- Modify: `crates/scout-web/src/routes/trips.rs`

- [ ] **Step 1: Write the failing tests**

In that file's `mod tests`, following the shape of the existing `choosing_a_candidate_updates_the_trip_and_requires_csrf`:

```rust
    #[tokio::test]
    async fn adding_a_leg_needs_a_session_and_a_csrf_token() {
        let (app, core, _dir, account, cookie, csrf) = signed_in_with_trip().await;
        let body = serde_json::json!({
            "trip": "October", "origin": "AMS", "destination": "LIS",
            "departure_date": "2026-10-12"
        });

        // No CSRF header: refused.
        let bare = post_json(&app, "/chat/trips/segment", &body, &cookie, None).await;
        assert_eq!(bare.status(), StatusCode::BAD_REQUEST);

        // With it: accepted.
        let ok = post_json(&app, "/chat/trips/segment", &body, &cookie, Some(&csrf)).await;
        assert_eq!(ok.status(), StatusCode::OK);
        let trip = core.store().find_trip(account, "October").unwrap().unwrap();
        assert!(trip.segments.iter().any(|s| s.destination == "LIS"));
    }

    #[tokio::test]
    async fn a_bad_airport_code_is_a_message_not_a_five_hundred() {
        let (app, _core, _dir, _account, cookie, csrf) = signed_in_with_trip().await;
        let body = serde_json::json!({
            "trip": "October", "origin": "Amsterdam", "destination": "LIS",
            "departure_date": "2026-10-12"
        });
        let res = post_json(&app, "/chat/trips/segment", &body, &cookie, Some(&csrf)).await;
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn a_stale_remove_is_a_conflict_and_changes_nothing() {
        // The browser's copy is out of date. It must be told to reload, not
        // silently delete whatever now sits at that position.
        let (app, core, _dir, account, cookie, csrf) = signed_in_with_trip().await;
        let trip = core.store().find_trip(account, "October").unwrap().unwrap();
        let before = trip.segments.len();

        let body = serde_json::json!({
            "trip": "October", "position": 0,
            "origin": "XXX", "destination": "YYY", "departure_date": "2026-10-12"
        });
        let res = delete_json(&app, "/chat/trips/segment", &body, &cookie, Some(&csrf)).await;
        assert_eq!(res.status(), StatusCode::CONFLICT);

        let after = core.store().find_trip(account, "October").unwrap().unwrap();
        assert_eq!(after.segments.len(), before, "a refused remove changes nothing");
    }
```

Reuse the existing test fixture rather than inventing one: read the current `mod tests` and copy its setup helper (the one that returns app, core, dir, account, cookie, csrf) and its request helpers, renaming as needed. Do not write new fixtures if equivalents exist.

- [ ] **Step 2: Run and watch them fail**

```bash
cargo test -p scout-web trips::tests
```

Expected: FAIL — 404, because the routes do not exist.

- [ ] **Step 3: Add the routes**

```rust
pub fn routes(auth: AuthState) -> Router {
    Router::new()
        .route("/chat/trips", get(list))
        .route("/chat/trips/choice", post(choose))
        .route("/chat/trips/segment", post(add_leg).delete(remove_leg))
        .layer(axum::middleware::from_fn_with_state(
            auth.clone(),
            super::only_from_our_own_pages,
        ))
        .with_state(auth)
}

#[derive(serde::Deserialize)]
struct AddLegIn {
    trip: String,
    position: Option<i64>,
    origin: String,
    destination: String,
    departure_date: String,
}

#[derive(serde::Deserialize)]
struct RemoveLegIn {
    trip: String,
    position: i64,
    origin: String,
    destination: String,
    departure_date: Option<String>,
}

/// Turns a leg edit into a response. Shared so add and remove cannot drift
/// into disagreeing about what a stale tab is told.
fn leg_response(out: scout_core::trips::LegEdit) -> Response {
    use scout_core::trips::LegEdit;
    match out {
        LegEdit::Done(plan) => axum::Json(plan).into_response(),
        LegEdit::TripNotFound => StatusCode::NOT_FOUND.into_response(),
        // Not an error: the reader's copy is simply older than the trip.
        LegEdit::SegmentChanged => StatusCode::CONFLICT.into_response(),
        LegEdit::Invalid(message) => {
            (StatusCode::UNPROCESSABLE_ENTITY, message).into_response()
        }
    }
}
```

The two handlers:

```rust
async fn add_leg(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<AddLegIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match scout_core::trips::add_leg(
        &auth.core, account_id, &body.trip, body.position,
        &body.origin, &body.destination, &body.departure_date,
    ).await {
        Ok(out) => leg_response(out),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not add a leg");
            sorry()
        }
    }
}

async fn remove_leg(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<RemoveLegIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match scout_core::trips::remove_leg(
        &auth.core, account_id, &body.trip, body.position,
        &body.origin, &body.destination, body.departure_date.as_deref(),
    ).await {
        Ok(out) => leg_response(out),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not remove a leg");
            sorry()
        }
    }
}
```

- [ ] **Step 4: Update the module doc**

The file's header currently says searching and **adding routes** happen in chat. Adding a leg no longer does. Amend it:

```rust
//! The visual trip planner's small JSON surface.
//!
//! It reads the itinerary Scout already stores, lets the traveller settle an
//! existing candidate, and edits the shape of the trip — adding and removing
//! legs, which are deterministic writes over data the store already holds.
//!
//! Searching and pricing still happen in chat: those spend money against a
//! live provider and stay the flight agent's responsibility.
```

- [ ] **Step 5: Run**

```bash
cargo test -p scout-web trips
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/scout-web/src/routes/trips.rs
git commit -m "feat: the trip view can add and remove a leg"
```

---

## Task 9: The composer knows where it is sending

**Files:**
- Modify: `crates/scout-web/src/chat.html`
- Modify: `crates/scout-web/src/chat.js`
- Test: `crates/scout-web/src/chat.test.mjs`

- [ ] **Step 1: Write the failing test**

In `chat.test.mjs`:

```js
test('the composer says which thread a trip message lands in', () => {
  // Three cases, because a trip's owner is not always somewhere the web
  // can post: an owned direct thread, an orphan, and a Telegram group.
  assert.deepEqual(composerTarget({ chat: { id: 7, title: 'Cheap flights in October', scope: 'direct' } }),
    { thread: 7, label: 'to “Cheap flights in October”', canSend: true })

  assert.deepEqual(composerTarget({ chat: { id: 7, title: null, scope: 'direct' } }),
    { thread: 7, label: 'to an unnamed thread', canSend: true })

  // Orphaned: sending starts a thread, which then adopts the trip.
  assert.deepEqual(composerTarget({ chat: null }),
    { thread: null, label: 'to a new chat', canSend: true })

  // A group is a room with other people in it. Offer a new direct thread
  // instead, and do not take the group's ownership away from it.
  assert.deepEqual(composerTarget({ chat: { id: 9, title: 'Trip crew', scope: 'telegram:-100' } }),
    { thread: null, label: 'planned in a Telegram group — replies go to a new chat', canSend: true })
})
```

- [ ] **Step 2: Run and watch it fail**

```bash
cd crates/scout-web/src && node --test chat.test.mjs
```

Expected: FAIL — `composerTarget is not defined`.

- [ ] **Step 3: Implement it**

In `chat.js`, beside the other exported pure helpers:

```js
// Where a message typed on the Trips tab goes, and what the line above the
// composer says. Pure so the three cases are testable without a DOM: an
// owned direct thread, an orphan, and a Telegram group the web must not
// post into. A group keeps its ownership — replying starts a separate
// direct thread rather than stealing the trip from the room that planned it.
export function composerTarget(trip) {
  const chat = trip?.chat
  if (!chat) return { thread: null, label: 'to a new chat', canSend: true }
  if (chat.scope !== 'direct') {
    return {
      thread: null,
      label: 'planned in a Telegram group — replies go to a new chat',
      canSend: true,
    }
  }
  return {
    thread: chat.id,
    label: chat.title ? `to “${chat.title}”` : 'to an unnamed thread',
    canSend: true,
  }
}
```

- [ ] **Step 4: Run**

```bash
cd crates/scout-web/src && node --test chat.test.mjs
```

Expected: PASS.

- [ ] **Step 5: Wire it into the page**

In `chat.html`, immediately above the `<form id="ask" class="ask">`:

```html
<p id="compose-target" class="compose-target" hidden></p>
```

and in the stylesheet, beside `.status`:

```css
  /* Names the thread a message will land in while the Trips tab is open.
     Muted, because it is a label on the composer and not a message. */
  .compose-target{font-size:13px; color:var(--base01); margin:0 0 6px; flex:none}
```

In `chat.js`'s `start()`, in `switchView`, after the existing lines:

```js
    // On Trips the composer addresses the selected trip's chat, so the
    // reader can see where a message goes before sending it.
    const trip = trips.find((t) => t.name === currentTrip)
    const target = showingTrips && trip ? composerTarget(trip) : null
    composeTarget.hidden = !target
    composeTarget.textContent = target ? `↩ ${target.label}` : ''
    tripThread = target ? target.thread : undefined
```

Declare `let tripThread` beside `let trips`, resolve `composeTarget` beside the other `getElementById` calls, and call the same block from wherever `currentTrip` changes so switching trips updates the line.

- [ ] **Step 6: Send to the right thread**

Where the composer submits, the thread it sends into becomes the trip's when the Trips tab is showing. `sendBody(text, thread)` already carries a thread id, so this is a choice of value and not new plumbing:

```js
    // On Trips the message belongs to the trip's own chat, not whichever
    // thread the Chat tab happens to be showing.
    const thread = tripThread !== undefined ? tripThread : currentThread
```

After sending, switch to the Chat tab so the answer streams where answers live, and reload trips when the run ends so the itinerary reflects whatever changed:

```js
    // Sent from Trips: go where answers live and watch it stream. The
    // itinerary reloads when the run ends, so whatever the answer changed
    // is on screen when the reader comes back to it.
    if (sentFromTrips) {
      switchView('chat')
      runFinished.then(() => loadTrips()).catch(() => {})
    }
```

`sentFromTrips` is `tripThread !== undefined`, captured *before* the view
switches — after it, the tab state no longer answers the question honestly.
`runFinished` is whatever promise the existing send path already resolves when
the stream's `end` frame arrives; reuse it rather than adding a second
completion signal.

- [ ] **Step 7: Run the whole JS suite**

```bash
cd crates/scout-web/src && node --test chat.test.mjs
```

Expected: all PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/scout-web/src/chat.js crates/scout-web/src/chat.html crates/scout-web/src/chat.test.mjs
git commit -m "feat: the composer names the chat a trip message lands in"
```

---

## Task 10: Leg controls in the trip view

**Files:**
- Modify: `crates/scout-web/src/chat.html`
- Modify: `crates/scout-web/src/chat.js`
- Test: `crates/scout-web/src/chat.test.mjs`

- [ ] **Step 1: Write the failing test**

```js
test('a remove sends what the reader actually saw', () => {
  // Positions renumber server-side, so the request carries the leg's
  // identity and not just its index. Without this the server cannot tell a
  // stale click from a current one.
  const segment = { position: 1, origin: 'LIS', destination: 'FCO', departure_date: '2026-10-14' }
  assert.deepEqual(JSON.parse(removeLegBody('Atlantic loop', segment)), {
    trip: 'Atlantic loop',
    position: 1,
    origin: 'LIS',
    destination: 'FCO',
    departure_date: '2026-10-14',
  })
})
```

- [ ] **Step 2: Run and watch it fail**

```bash
cd crates/scout-web/src && node --test chat.test.mjs
```

Expected: FAIL — `removeLegBody is not defined`.

- [ ] **Step 3: Implement**

```js
// The remove request. It carries the leg's identity, not only its index,
// because `drop_segment` renumbers: by the time a click arrives, position 1
// may be a different flight than the one the reader was looking at.
export function removeLegBody(tripName, segment) {
  return JSON.stringify({
    trip: tripName,
    position: segment.position,
    origin: segment.origin,
    destination: segment.destination,
    departure_date: segment.departure_date ?? null,
  })
}
```

- [ ] **Step 4: Add the controls**

In the segment rendering in `chat.js`, add a remove button per segment and an add-leg form at the end of the itinerary. Follow the markup conventions already in `#trips-view`. The handlers:

```js
  async function removeLeg(tripName, segment) {
    const res = await fetch('/chat/trips/segment', {
      method: 'DELETE',
      headers: { 'content-type': 'application/json', 'x-csrf': csrfToken },
      body: removeLegBody(tripName, segment),
    })
    if (res.status === 409) {
      // Not an error: this tab's copy is simply older than the trip.
      showTripNotice('That leg changed while you were looking. Reloading.')
      await loadTrips()
      return
    }
    if (!res.ok) {
      showTripNotice('Could not remove that leg.')
      return
    }
    applyTrip(await res.json())
  }
```

Use the existing header name for CSRF — read `csrf_header_ok` in `crates/scout-web/src/routes/chat.rs` for the exact name rather than assuming `x-csrf`, and reuse whatever the existing choice-sending code already sends.

`applyTrip` is the existing function that takes an authoritative post-write trip and repaints; if it is named differently, use the real name — the candidate-choice path already does exactly this.

- [ ] **Step 5: Run**

```bash
cd crates/scout-web/src && node --test chat.test.mjs
```

Expected: PASS.

- [ ] **Step 6: Look at it**

There is no dev server in this repo by default. Build a static harness from the real template, the way the status-pane work did:

```bash
SP=/tmp/trip-harness && mkdir -p $SP
python3 - "$SP" <<'PY'
import io, sys
sp = sys.argv[1]
s = io.open('crates/scout-web/src/chat.html', encoding='utf-8').read()
s = s.replace('<script type="module" src="/chat.js"></script>', '').replace('<!--MIRROR-->', 'false')
io.open(f'{sp}/harness.html', 'w', encoding='utf-8').write(s)
PY
(cd $SP && python3 -m http.server 8799 --bind 127.0.0.1)
```

Then open `http://127.0.0.1:8799/harness.html` and confirm the composer target line and the leg controls sit correctly at both a narrow and a wide viewport. Kill the server afterwards.

- [ ] **Step 7: Commit**

```bash
git add crates/scout-web/src/chat.js crates/scout-web/src/chat.html crates/scout-web/src/chat.test.mjs
git commit -m "feat: add and remove a leg from the trip view"
```

---

## Task 11: The whole suite, and the deploy conversation

**Files:** none

- [ ] **Step 1: Full suite, both languages**

```bash
cargo test --workspace > /tmp/full.out 2>&1; echo "cargo exit: $?"
grep -E "^test result" /tmp/full.out
(cd crates/scout-web/src && node --test chat.test.mjs | grep -E "^# (pass|fail)")
```

Expected: `cargo exit: 0`, every line `ok`, `# fail 0`.

**Do not pipe `cargo test` through `tail` or `head`.** It truncates all but the last crate's results and replaces cargo's exit code with the pager's — a green-looking run that proves nothing.

- [ ] **Step 2: Clippy**

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo clippy --workspace --all-targets 2>&1 | grep -E "^(warning|error)" | grep -v "future version of Rust"
```

Expected: no output.

- [ ] **Step 3: Confirm the two guards are still guarding**

Re-run the two mutation checks from Tasks 2 and 4 one final time, since later tasks touched the same file. Both must go red when the guard is removed and green when it is restored.

- [ ] **Step 4: Stop before deploying and say this out loud**

This branch carries schema step 9. Before it goes to production:

- `apply_steps` takes an automatic snapshot before the first step, verified working — the 6→8 migration left `scout-2026-09-05T142740Z-migration-v8.duckdb` on the volume.
- That snapshot, and every nightly, **lives on the same disk as the database**. The off-site backup CronJob has never completed successfully.
- The R2 credentials are wrong: `AWS_ACCESS_KEY_ID` holds a Cloudflare Token value and `AWS_SECRET_ACCESS_KEY` holds the account id. The token also needs rolling, because its derived secret leaked into a transcript.

Only the account holder can mint the replacement. Report this and let them decide whether to fix it first; do not deploy a migration silently.

---

## Self-review

**Spec coverage.** Purpose → Tasks 1–10. Lifetimes/detach → Task 4, with a mutation check. Link and adoption → Tasks 1, 2, 5. Editing legs → Tasks 7, 8, 10, with the `ExpectedSegment` guard in 7. Composer → Tasks 6, 9. Migration → Task 1 (both paths). Testing section → the tests are distributed into the tasks that create their subjects. "Not building" honoured: no live re-search, no delete-trip control.

**Known soft spots the implementer must resolve by reading, not guessing:** the exact CSRF header name, the real test fixture helpers in `routes/trips.rs`, the true name of the thread-title setter, the real fields of `AddSegmentArgs`, whether `Store::conn()` is re-entrant (Task 7 Step 3 depends on it), and the existing repaint function's name. Each is flagged at its step.

**Type consistency.** `LegEdit` is defined once (Task 7) and used in Task 8. `TripChat` is defined in Task 6 and consumed by `composerTarget` in Task 9 as `{id, title, scope}`. `upsert_trip`'s fifth parameter is `Option<i64>` in Tasks 2, 5 and every test. `remove_segment_checked` returns `bool` in Task 7 and is mapped to `SegmentChanged` in the same task.
