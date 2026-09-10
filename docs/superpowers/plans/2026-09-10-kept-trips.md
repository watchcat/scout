# Kept Trips Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The flight specialist builds a trip on every search, invisibly, and the traveller keeps the ones worth keeping.

**Architecture:** One boolean column, `trips.kept`. New trips are drafts; the migration backfills every existing trip to kept, because creating one used to *be* the act of keeping it. The model's reads see drafts (or it would build a second trip next message); the traveller's read does not. A `keep_trip` tool flips the bit. Expiry deletes unkept drafts and detaches kept trips — a timer must not destroy a plan, but a draft nobody kept is exactly what a timer should clean up.

**Tech Stack:** Rust, DuckDB behind a `Mutex`, rig 0.40 agents/tools, axum 0.8.

---

## Read this first

**This repository is deliberately not rustfmt-formatted. Never run `cargo fmt`.** Match the hand-formatting of the file you are editing.

**Clippy needs the rustup toolchain:**

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo clippy --workspace --all-targets
```

Plain `cargo clippy` fails with `E0514`. That is a known environment quirk, not your bug.

**The build is warning-free and the suite is green (840 Rust + 33 JS). Keep it that way.** No `#[allow(dead_code)]`, no `#[allow(...)]` of any kind.

**Comments explain *why*, recording the failure that motivated the line.** Read the neighbours before writing one — the comments in `store.rs`, `specialist.rs` and `tools/trips.rs` are the standard to match. Never restate the code.

**Test names are full sentences about behaviour.**

**The fixture is `test_store()` returning `(Store, TempDir)`.** There is no `temp_store`.

**Positions on trip segments are 1-based** (`add_segment` computes `count + 1`).

**A new column needs two edits.** `Store::open` runs `execute_batch(MIGRATIONS)` on every open, but `CREATE TABLE IF NOT EXISTS` does nothing to a table that already exists. So the column goes in *both* the `CREATE TABLE` (fresh databases) and a numbered `ALTER TABLE` step (existing ones). `steps()` currently ends at 9; yours is **10**.

**Column order matters.** `ALTER TABLE ... ADD COLUMN` appends, so the new column must be written **last** in the `CREATE TABLE` — after `conversation_id`, before the `UNIQUE` constraint. There is a test, `a_migrated_trips_table_has_exactly_the_shape_a_fresh_one_has`, that will catch you if you get this wrong. Do not "fix" that test.

**Steps must be idempotent.** Use `ADD COLUMN IF NOT EXISTS`, as steps 7 and 9 do. Several test fixtures build a database via `MIGRATIONS` (which creates the finished shape) and then record an older `schema_version`, so a step can meet a column that already exists. A bare `ADD COLUMN` breaks those fixtures.

---

## File structure

| File | Responsibility | Change |
|---|---|---|
| `crates/scout-core/src/store.rs` | schema, all SQL | `kept` column, `STEP_10`, `Trip.kept`, `load_trip`, `list_kept_trips`, `keep_trip`, expiry split |
| `crates/scout-core/src/trips.rs` | channel-facing API | `list()` reads kept only |
| `crates/scout-core/src/tools/trips.rs` | the model's tools | `KeepTripTool` |
| `crates/scout-core/src/flights.rs` | specialist wiring + guidance | register the tool, offer line, preamble |

---

## Task 1: The column, the migration, and the backfill

**Files:**
- Modify: `crates/scout-core/src/store.rs` — trips `CREATE TABLE` (~line 90), `STEP_9_TRIP_CONVERSATION` (~line 751), `steps()` (~line 765), `Trip` (~line 293), `load_trip`

- [ ] **Step 1: Write the failing tests**

Add to `store.rs`'s `mod tests`:

```rust
    #[test]
    fn a_trip_the_model_builds_starts_as_a_draft() {
        // Building a trip is now free and automatic, so creating one is no
        // longer the act of intent it used to be. Keeping it is.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Atlantic loop", None, None, None).unwrap();
        assert!(!trip.kept, "a newly built trip is a draft until the traveller keeps it");
    }
```

And the one that matters, in the same module:

```rust
    #[test]
    fn every_trip_that_already_existed_is_kept_by_the_migration() {
        // Every trip in a database written before this column existed was
        // created under rules where making one *was* keeping it. Without the
        // backfill, this migration hides every trip a traveller already has
        // — which is the exact complaint the feature exists to answer,
        // inflicted on all their existing data.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scout.duckdb");
        {
            let conn = duckdb::Connection::open(&path).unwrap();
            conn.execute_batch(PRE_KEPT_TRIPS).unwrap();
            conn.execute_batch(
                "INSERT INTO trips (account_id, name, name_key) VALUES (1, 'Japan in spring', 'japan in spring');
                 CREATE TABLE schema_version (version BIGINT NOT NULL);
                 INSERT INTO schema_version VALUES (9);",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let kept: bool = store
            .conn()
            .query_row("SELECT kept FROM trips WHERE name_key = 'japan in spring'", [], |r| r.get(0))
            .unwrap();
        assert!(kept, "a trip that predates the column must survive as kept, not vanish");
    }
```

`PRE_KEPT_TRIPS` is a frozen schema-9 `trips` table you must define beside the existing frozen consts. Follow the convention of `LEGACY_SCHEMA` and `PRE_TRIP_CONVERSATION_TRIPS` — a named const with a comment saying it must never be updated when `MIGRATIONS` changes, because its whole value is being an honest picture of the database the migration will meet:

```rust
/// The `trips` table as it stood at schema 9, before `kept`. Do NOT update
/// this when `MIGRATIONS` changes — its value is being an honest picture of
/// the database step 10 will actually meet.
const PRE_KEPT_TRIPS: &str = r#"
CREATE SEQUENCE IF NOT EXISTS trips_id_seq;
CREATE TABLE trips (
    id          BIGINT PRIMARY KEY DEFAULT nextval('trips_id_seq'),
    account_id     BIGINT NOT NULL,
    name        TEXT NOT NULL,
    name_key    TEXT NOT NULL,
    adults      BIGINT NOT NULL DEFAULT 1,
    cabin_class TEXT,
    status      TEXT NOT NULL DEFAULT 'planning',
    created_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    conversation_id BIGINT,
    UNIQUE (account_id, name_key)
);
"#;
```

If `Store::open` fails on this hand-built database for an unrelated reason (a table some other step expects), add what it needs — do **not** weaken the assertion, and do not delete the test. If you cannot make it work after a genuine attempt, report BLOCKED with the exact error.

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p scout-core a_trip_the_model_builds_starts_as_a_draft
cargo test -p scout-core every_trip_that_already_existed_is_kept
```

Expected: FAIL — no field `kept` on `Trip`, and `Binder Error: Referenced column "kept" not found`.

- [ ] **Step 3: Add the column, last in the CREATE TABLE**

In `MIGRATIONS`, the `trips` table — after `conversation_id`, before the `UNIQUE`:

```sql
    conversation_id BIGINT,
    -- Last, so a fresh database and a migrated one — where this arrives by
    -- ALTER TABLE in step 10 — have the same column order. False means a
    -- draft: built automatically while searching, invisible to the
    -- traveller until they ask to keep it.
    kept        BOOLEAN NOT NULL DEFAULT false,
    UNIQUE (account_id, name_key)
```

- [ ] **Step 4: Add step 10, with its backfill**

After `STEP_9_TRIP_CONVERSATION`:

```rust
/// `kept` marks a trip the traveller asked to keep. It defaults false —
/// new trips are drafts — but every trip already in the database was made
/// when creating one *was* the act of keeping it, so they are all kept.
/// Without that UPDATE this step hides every trip a traveller already has.
///
/// `IF NOT EXISTS` for the same reason step 7 has it: a fixture can build
/// the finished shape from `MIGRATIONS` and then record an older version,
/// so this step can meet a column that is already there. Note the UPDATE
/// still runs in that case, which is correct — such a database has no
/// drafts in it to protect.
const STEP_10_KEPT_TRIPS: &str = r#"
ALTER TABLE trips ADD COLUMN IF NOT EXISTS kept BOOLEAN NOT NULL DEFAULT false;
UPDATE trips SET kept = true;
"#;
```

Register it:

```rust
        (9, Step::Sql(STEP_9_TRIP_CONVERSATION)),
        (10, Step::Sql(STEP_10_KEPT_TRIPS)),
```

- [ ] **Step 5: Carry it on `Trip`**

`Trip` is the model-facing type. Add the field last, so the JSON the model reads keeps its existing field order:

```rust
    pub segments: Vec<TripSegment>,
    /// False while this is a draft the specialist built as it searched.
    /// The model sees this — it needs to know whether to offer to keep it.
    pub kept: bool,
```

Then make `load_trip` read it. Find the `SELECT` it uses for the trip row and add the column; adjust the row indices of everything after it if you insert rather than append.

- [ ] **Step 6: Fix the compiler errors**

Every place that constructs a `Trip` literal now needs `kept`. Let the compiler list them and fix each. Test constructions get whatever the test is about — do not blanket-fill `true`.

- [ ] **Step 7: Run both tests, then the crate**

```bash
cargo test -p scout-core a_trip_the_model_builds_starts_as_a_draft
cargo test -p scout-core every_trip_that_already_existed_is_kept
cargo test -p scout-core 2>&1 | grep -E "^test result|^error"
```

Expected: both new tests PASS, whole crate ok.

- [ ] **Step 8: Prove the backfill is load-bearing**

Delete the `UPDATE trips SET kept = true;` line from `STEP_10_KEPT_TRIPS` and run:

```bash
cargo test -p scout-core every_trip_that_already_existed_is_kept
```

Expected: **FAIL** on "a trip that predates the column must survive as kept, not vanish". Restore the line and confirm it passes.

Report the verbatim result. If it stays green, the test is not testing the backfill and must be fixed before you continue.

- [ ] **Step 9: Commit**

```bash
git add crates/scout-core/src/store.rs
git commit -m "feat: a trip is a draft until it is kept"
```

---

## Task 2: The traveller's list shows kept trips; the model's shows everything

**Files:**
- Modify: `crates/scout-core/src/store.rs` — `list_trips` (~line 2622)
- Modify: `crates/scout-core/src/trips.rs` — `list()`

This is the mechanism of the whole feature, and it is a boundary the code already draws: `Trip` is model-facing, `Plan` is channel-facing.

- [ ] **Step 1: Write the failing test**

In `trips.rs`'s `mod tests`:

```rust
    #[tokio::test]
    async fn the_trips_tab_shows_kept_trips_and_not_the_drafts_built_while_searching() {
        // The regression test for the reported bug, from both ends: a draft
        // exists in the store and does not reach the traveller. Without the
        // second half, every casual price check litters the Trips tab, which
        // is the reason drafts exist at all.
        let (core, _dir) = temp_core().await;
        let account = core.store().account_for_telegram(1).unwrap();
        core.store().upsert_trip(account, "Draft loop", None, None, None).unwrap();
        let kept = core.store().upsert_trip(account, "Japan in spring", None, None, None).unwrap();
        core.store().keep_trip(account, "Japan in spring").unwrap();

        let plans = list(&core, account).await.unwrap();
        let names: Vec<&str> = plans.iter().map(|p| p.trip.name.as_str()).collect();
        assert_eq!(names, vec!["Japan in spring"], "a draft is not the traveller's business yet");
        let _ = kept;
    }
```

Check `temp_core` is the real helper name in that file before using it; if the file stands a `Core` up differently, follow what it does.

This test depends on `Store::keep_trip`, which Task 3 builds. Write `keep_trip` now as the minimal store method (the tool comes later) so this task is testable on its own:

```rust
    /// Marks a trip the traveller asked to keep, so it appears in their
    /// list. Idempotent: keeping a kept trip is a no-op that says it found
    /// the trip, because "keep this" twice is not an error.
    pub fn keep_trip(&self, account_id: i64, name: &str) -> Result<bool> {
        let key = name.trim().to_lowercase();
        let conn = self.conn();
        let found: i64 = conn.query_row(
            "SELECT count(*) FROM trips WHERE account_id = ? AND name_key = ?",
            params![account_id, key],
            |row| row.get(0),
        )?;
        if found == 0 {
            return Ok(false);
        }
        conn.execute(
            "UPDATE trips SET kept = true, updated_at = current_timestamp
             WHERE account_id = ? AND name_key = ?",
            params![account_id, key],
        )?;
        Ok(true)
    }
```

Note this one *does* bump `updated_at`, unlike adoption: keeping is an act of the traveller's, and the list is ordered by `updated_at DESC`, so a trip they just kept should be at the top.

- [ ] **Step 2: Run and watch it fail**

```bash
cargo test -p scout-core the_trips_tab_shows_kept_trips
```

Expected: FAIL — the draft is in the list.

- [ ] **Step 3: Add the kept-only read**

Beside `list_trips` in `store.rs`. A sibling, not a boolean parameter — the model's read and the traveller's read are different questions, and a flag is something a caller can get backwards:

```rust
    /// The trips the traveller asked to keep, newest activity first.
    ///
    /// The channel-facing read. `list_trips` is the model's, and includes
    /// drafts on purpose: a specialist that could not see the trip it just
    /// built would build a second one on the next message.
    pub fn list_kept_trips(&self, account_id: i64) -> Result<Vec<Trip>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id FROM trips WHERE account_id = ? AND kept
             ORDER BY updated_at DESC, id DESC",
        )?;
        let ids: Vec<i64> = stmt
            .query_map(params![account_id], |row| row.get(0))?
            .collect::<duckdb::Result<_>>()?;
        ids.into_iter().map(|id| load_trip(&conn, id)).collect()
    }
```

Then in `trips.rs`'s `list()`, change `store.list_trips(account_id)` to `store.list_kept_trips(account_id)`. Leave `trip_names` and `ShowTripTool` on `list_trips` — they are the model's.

- [ ] **Step 4: Write the other half**

The model must still see its own draft, or it builds a second trip next message:

```rust
    #[test]
    fn the_model_still_sees_the_draft_it_just_built() {
        // `trip_names` and `show_trip` feed the model. A specialist that
        // cannot find the trip it built a moment ago builds another one,
        // and the traveller ends up with two half-itineraries.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        store.upsert_trip(account, "Draft loop", None, None, None).unwrap();
        let names: Vec<String> =
            store.list_trips(account).unwrap().into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["Draft loop".to_string()]);
        assert!(store.list_kept_trips(account).unwrap().is_empty());
    }
```

- [ ] **Step 5: Run**

```bash
cargo test -p scout-core the_trips_tab_shows_kept_trips
cargo test -p scout-core the_model_still_sees_the_draft
cargo test --workspace 2>&1 | grep -E "^test result|^error"
```

Expected: all PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/scout-core/src/store.rs crates/scout-core/src/trips.rs
git commit -m "feat: the trips tab shows what the traveller kept"
```

---

## Task 3: The `keep_trip` tool

**Files:**
- Modify: `crates/scout-core/src/tools/trips.rs`
- Modify: `crates/scout-core/src/flights.rs` — register it

- [ ] **Step 1: Write the failing test**

In `tools/trips.rs`'s `mod tests`, following the shape of the tests already there:

```rust
    #[tokio::test]
    async fn keeping_a_trip_makes_it_the_travellers_and_saying_it_twice_is_fine() {
        // "Keep this" twice is a traveller repeating themselves, not an
        // error worth a sentence.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        store.upsert_trip(account, "Atlantic loop", None, None, None).unwrap();
        let tool = KeepTripTool { store: store.clone(), account_id: account };

        let view = tool.call(KeepTripArgs { trip: "Atlantic loop".to_string() }).await.unwrap();
        assert!(view.trip.kept);
        let again = tool.call(KeepTripArgs { trip: "atlantic loop".to_string() }).await.unwrap();
        assert!(again.trip.kept, "keeping a kept trip is a no-op, not a failure");
    }

    #[tokio::test]
    async fn keeping_a_trip_that_is_not_there_names_the_ones_that_are() {
        // A mistyped name is indistinguishable from one never created, so
        // both get the real names to correct against — the same treatment
        // `delete_trip` gives.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        store.upsert_trip(account, "Atlantic loop", None, None, None).unwrap();
        let tool = KeepTripTool { store: store.clone(), account_id: account };
        let err = tool.call(KeepTripArgs { trip: "Japan".to_string() }).await.unwrap_err();
        assert!(err.to_string().contains("Atlantic loop"), "got: {err}");
    }
```

Check `TripView`'s real field names before asserting on `view.trip.kept` — read the struct.

- [ ] **Step 2: Run and watch it fail**

```bash
cargo test -p scout-core keeping_a_trip_makes_it_the_travellers
```

Expected: FAIL to compile — no `KeepTripTool`.

- [ ] **Step 3: Implement the tool**

Copy the shape of `DeleteTripTool` (~line 1388) exactly — same struct fields, same `Tool` impl layout, same `spawn_blocking` pattern, same use of `trip_names` for the not-found message.

```rust
#[derive(Debug, serde::Deserialize)]
pub struct KeepTripArgs {
    pub trip: String,
}

pub struct KeepTripTool {
    pub store: Store,
    pub account_id: i64,
}

impl Tool for KeepTripTool {
    const NAME: &'static str = "keep_trip";
    type Error = StoreToolError;
    type Args = KeepTripArgs;
    type Output = TripView;

    fn description(&self) -> String {
        "Keep a trip, so it appears in the traveller's saved trips. Trips you \
         build while searching are drafts they cannot see; call this only when \
         the traveller has said they want this one kept. Costs nothing and \
         searches nothing."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {"trip": {"type": "string", "description": "the trip's name"}},
            "required": ["trip"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let store = self.store.clone();
        let account_id = self.account_id;
        tokio::task::spawn_blocking(move || -> anyhow::Result<TripView> {
            if !store.keep_trip(account_id, &args.trip)? {
                anyhow::bail!(
                    "no trip called {:?} to keep. This traveller has: {}",
                    args.trip,
                    trip_names(&store, account_id)?
                );
            }
            let trip = find_trip_or_list(&store, account_id, &args.trip)?;
            Ok(TripView::of(trip))
        })
        .await
        .map_err(|e| StoreToolError(e.to_string()))?
        .map_err(|e| StoreToolError(e.to_string()))
    }
}
```

`TripView::of` may not exist — read how the other tools build a `TripView` from a `Trip` and use whatever they use.

- [ ] **Step 4: Register it on the flight specialist**

In `flights.rs`, beside `.tool(DeleteTripTool { ... })`:

```rust
        .tool(KeepTripTool { store: d.store.clone(), account_id })
```

and add `KeepTripTool` to the `use` list at the top (~line 12), and `"keep_trip"` to `TRIP_TOOLS` (~line 27) so a keep counts as trip work for guidance.

- [ ] **Step 5: Run**

```bash
cargo test -p scout-core keeping_a_trip
cargo test --workspace 2>&1 | grep -E "^test result|^error"
PATH="$HOME/.cargo/bin:$PATH" cargo clippy --workspace --all-targets 2>&1 | grep -E "^(warning|error)" | grep -v "future version of Rust"
```

Expected: PASS, no clippy output.

- [ ] **Step 6: Commit**

```bash
git add crates/scout-core/src/tools/trips.rs crates/scout-core/src/flights.rs
git commit -m "feat: the traveller can keep a trip the search built"
```

---

## Task 4: A timer deletes a draft and detaches a plan

**Files:**
- Modify: `crates/scout-core/src/store.rs` — `expire_conversations` (~line 1591), `detach_trips_within` (~line 2954)

The trip↔chat link established that expiry **detaches** rather than deletes, because a timer must never destroy a plan the traveller is still building. A draft is not a plan — nobody kept it. This task draws that line.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn expiry_deletes_the_draft_and_lets_the_kept_trip_go_free() {
        // Both outcomes in one test on purpose: split into two, either can
        // be satisfied by treating every trip alike, which is precisely the
        // mistake this distinction exists to prevent. A timer must never
        // destroy a plan — and a draft nobody kept is exactly what a timer
        // should clean up.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let stale = store.start_conversation(account, "direct").unwrap();

        let draft = store.upsert_trip(account, "Draft loop", None, None, Some(stale)).unwrap();
        let plan = store.upsert_trip(account, "Japan in spring", None, None, Some(stale)).unwrap();
        store.keep_trip(account, "Japan in spring").unwrap();

        store
            .conn()
            .execute_batch(&format!(
                "UPDATE conversations
                 SET updated_at = CAST(current_timestamp AS TIMESTAMP) - INTERVAL 72 HOUR
                 WHERE id = {stale}"
            ))
            .unwrap();

        assert_eq!(store.expire_conversations(48 * 3600, &[]).unwrap(), 1);

        assert!(
            store.find_trip(account, "Draft loop").unwrap().is_none(),
            "an unkept draft goes with the thread that made it"
        );
        let kept = store.find_trip(account, "Japan in spring").unwrap();
        assert!(kept.is_some(), "a timer must never destroy a plan the traveller kept");
        assert_eq!(store.trip_owner(plan.id).unwrap(), None, "and its dead link is released");
        let _ = draft;
    }
```

Check how the existing expiry tests age a conversation and copy that idiom exactly. The repo documents a DuckDB failure where `to_seconds(CAST(? AS INTEGER))` fails to bind on a cold connection (`store.rs` ~1984-1999), which is why the existing tests use a formatted literal interval. Do not use a bound parameter here.

`trip_owner` is `#[cfg(test)]`-gated; it is available inside `mod tests`.

- [ ] **Step 2: Run and watch it fail**

```bash
cargo test -p scout-core expiry_deletes_the_draft
```

Expected: FAIL on "an unkept draft goes with the thread that made it" — today expiry detaches everything, so the draft survives.

- [ ] **Step 3: Delete drafts before detaching the rest**

In `expire_conversations`, immediately before the existing `detach_trips_within(&conn, &doomed_ids)?;`:

```rust
            // A draft is not a plan: nobody kept it, and the thread it was
            // built in is gone. This is the one place a timer may remove a
            // trip, and it is why `detach_trips_within` below still exists —
            // what the traveller kept is released, not destroyed.
            delete_drafts_within(&conn, &doomed_ids)?;
```

And beside `detach_trips_within`:

```rust
/// Removes the unkept drafts owned by these conversations, with their
/// segments and parked options.
///
/// Children before parents, or the subquery finds nothing. Guards the empty
/// slice for the same reason `detach_trips_within` does: `IN ()` is a parser
/// error, and the ordinary hourly sweep expires nothing.
fn delete_drafts_within(conn: &Connection, conversation_ids: &[i64]) -> Result<usize> {
    if conversation_ids.is_empty() {
        return Ok(0);
    }
    let holes = ["?"].repeat(conversation_ids.len()).join(", ");
    let doomed = format!(
        "(SELECT id FROM trips WHERE NOT kept AND conversation_id IN ({holes}))"
    );
    conn.execute(
        &format!("DELETE FROM segment_candidates WHERE trip_id IN {doomed}"),
        duckdb::params_from_iter(conversation_ids.iter()),
    )?;
    conn.execute(
        &format!("DELETE FROM trip_segments WHERE trip_id IN {doomed}"),
        duckdb::params_from_iter(conversation_ids.iter()),
    )?;
    Ok(conn.execute(
        &format!("DELETE FROM trips WHERE NOT kept AND conversation_id IN ({holes})"),
        duckdb::params_from_iter(conversation_ids.iter()),
    )?)
}
```

- [ ] **Step 4: Run**

```bash
cargo test -p scout-core expiry_deletes_the_draft
cargo test -p scout-core expired
```

Expected: PASS, and the existing expiry tests still pass.

- [ ] **Step 5: MUTATION CHECK — mandatory**

Remove `NOT kept AND` from the final `DELETE FROM trips` in `delete_drafts_within` and run:

```bash
cargo test -p scout-core expiry_deletes_the_draft
```

Expected: **FAIL** on "a timer must never destroy a plan the traveller kept". Restore, confirm green.

Then the other direction: replace the `delete_drafts_within(...)` call with nothing and re-run. Expected: FAIL on "an unkept draft goes with the thread that made it". Restore.

Report both verbatim. Two mutations because this test asserts two opposite things and either half could be vacuous.

- [ ] **Step 6: Commit**

```bash
git add crates/scout-core/src/store.rs
git commit -m "fix: a timer clears the drafts nobody kept and frees the plans they did"
```

---

## Task 5: The offer, computed rather than hoped for

**Files:**
- Modify: `crates/scout-core/src/flights.rs` — `guidance()` (~line 215), the preamble (~line 67)

The current bug is a guidance sentence the model did not act on. Writing another sentence and hoping harder repeats it. `guidance()` already computes `planned` in Rust and hands the result to the parent **as data alongside its findings** — that is the mechanism to use.

- [ ] **Step 1: Write the failing test**

In `flights.rs`'s `mod tests`, following how the existing guidance tests build findings:

```rust
    #[test]
    fn a_draft_is_offered_and_a_kept_trip_is_not() {
        // The offer is computed here rather than left to the model, because
        // the bug this fixes was a preamble sentence it did not act on.
        let draft = finding("add_trip_segment", json!({"trip": {"name": "HKG-FUK Sep", "kept": false}}));
        let text = guidance(&[draft], 0.0).join(" ");
        assert!(text.contains("HKG-FUK Sep"), "the offer must name the trip: {text}");
        assert!(text.to_lowercase().contains("keep"), "got: {text}");

        let kept = finding("add_trip_segment", json!({"trip": {"name": "HKG-FUK Sep", "kept": true}}));
        let text = guidance(&[kept], 0.0).join(" ");
        assert!(!text.to_lowercase().contains("keep"), "a kept trip needs no offer: {text}");

        // No trip built at all: nothing to offer.
        let searched = finding("search_flights", json!({"by_date": []}));
        let text = guidance(&[searched], 0.0).join(" ");
        assert!(!text.to_lowercase().contains("keep"), "got: {text}");
    }
```

`finding(...)` is a helper you may need to write or adapt — read the existing guidance tests first and reuse whatever they use to build a `Finding`. Match the real JSON shape a trip tool's output has: read `TripView`'s `Serialize` output rather than assuming it is `{"trip": {...}}`.

- [ ] **Step 2: Run and watch it fail**

```bash
cargo test -p scout-core a_draft_is_offered_and_a_kept_trip_is_not
```

Expected: FAIL — no offer in the guidance.

- [ ] **Step 3: Compute the offer**

In `guidance()`, beside the existing `planned`:

```rust
    // The name of a trip that was built but not kept, if there is one. The
    // parent cannot offer to keep something it cannot name, and it has no
    // other way to learn the name — it never sees the trip tools.
    let draft = ok()
        .filter(|f| TRIP_TOOLS.contains(&f.tool.as_str()))
        .find_map(|f| {
            let trip = f.output.get("trip")?;
            match trip.get("kept")?.as_bool()? {
                true => None,
                false => Some(trip.get("name")?.as_str()?.to_string()),
            }
        });
```

and where the guidance lines are pushed:

```rust
    if let Some(name) = draft {
        out.push(format!(
            "This trip is saved as a draft called {name:?} and the traveller cannot see it \
             yet. Show them the itinerary and ask whether to keep it; if they say yes, \
             send a brief saying to keep the trip called {name:?}."
        ));
    }
```

Adjust the JSON path (`f.output.get("trip")`) to the real serialized shape.

- [ ] **Step 4: Make the preamble unconditional**

The preamble at ~line 67 currently hedges: *"When the brief is about more than one flight — a multi-city route, or a trip being assembled over several messages — build it with the trip tools"*. That hedge is why nothing was built for a plain return search.

Rewrite that opening clause so building is unconditional, and say why it is safe:

```
- Build the trip with the trip tools every time you search, not only for \
multi-city routes: add_trip_segment for each leg the moment you have a \
route and a date, add_trip_option for each flight found. It costs nothing \
and the traveller does not see it — a trip you build is a draft until they \
ask to keep it, so there is no such thing as building one needlessly. \
```

Keep the rest of that bullet — the return-is-two-segments rule, the never-delete-and-rebuild rule, the finalise rule — exactly as it is. Only the trigger changes.

- [ ] **Step 5: Run**

```bash
cargo test -p scout-core guidance
cargo test --workspace 2>&1 | grep -E "^test result|^error"
```

- [ ] **Step 6: Commit**

```bash
git add crates/scout-core/src/flights.rs
git commit -m "feat: the findings tell the parent there is a draft to offer"
```

---

## Task 6: The whole suite, and the deploy conversation

**Files:** none

- [ ] **Step 1: Full suite, both languages, honest exit code**

```bash
cargo test --workspace > /tmp/full.out 2>&1; echo "cargo exit: $?"
grep -E "^test result" /tmp/full.out
(cd crates/scout-web/src && node --test chat.test.mjs | grep -E "^# (pass|fail)")
```

Expected: `cargo exit: 0`, every line `ok`, `# fail 0`.

**Do not pipe `cargo test` through `tail` or `head`.** It truncates all but the last crate's results *and* replaces cargo's exit code with the pager's — a green-looking run that proves nothing.

- [ ] **Step 2: Clippy**

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo clippy --workspace --all-targets 2>&1 | grep -E "^(warning|error)" | grep -v "future version of Rust"
```

Expected: no output.

- [ ] **Step 3: Re-run the two guards**

Tasks 1 and 4 each carry a mutation check, and later tasks touched the same file. Re-run both. Each must go red when its guard is removed and green when restored.

- [ ] **Step 4: Stop before deploying and say this**

This branch carries schema step 10, and its `UPDATE trips SET kept = true` touches every existing row.

- `apply_steps` snapshots before the first step — verified working in production; the 8→9 migration left `scout-2026-09-09T212058Z-migration-v9.duckdb` on the volume.
- That snapshot and every nightly **live on the same disk as the database**. The off-site backup CronJob has never completed successfully.
- `AWS_ACCESS_KEY_ID` holds a Cloudflare Token value and `AWS_SECRET_ACCESS_KEY` holds the account id. The token needs rolling as well as replacing, because its derived secret leaked into a transcript.

Only the account holder can mint the replacement. Report this and let them decide; do not deploy a migration silently.

---

## Self-review

**Spec coverage.** The column and its backfill → Task 1. Who sees a draft → Task 2. Keeping → Tasks 2 (store) and 3 (tool). What a timer does → Task 4, with two mutation checks because the test asserts two opposite things. The offer → Task 5, computed in Rust as the spec requires. The preamble → Task 5 Step 4. Testing section: every bullet maps to a task; the migration-backfill test is Task 1 and is the one the spec singles out as unprovable by a fresh-database test.

**Not building, honoured:** no web keep button, no automatic promotion on booking or finalising, no live re-pricing.

**Deferred in the spec and still deferred:** naming collisions under `UNIQUE (account_id, name_key)`, and drafts accumulating in a thread that never expires. Neither has a task, deliberately.

**Soft spots the implementer must resolve by reading, not guessing:** the real `Core` test helper name in `trips.rs`, `TripView`'s serialized shape (Task 5's JSON path depends on it), how `TripView` is constructed from a `Trip`, the `Finding` helper in the guidance tests, and `load_trip`'s row indices. Each is flagged at its step.

**Type consistency.** `Store::keep_trip(account_id, name) -> Result<bool>` is defined in Task 2 and used by Tasks 3 and 4. `list_kept_trips` is defined in Task 2 and used only by `trips::list`. `KeepTripTool`/`KeepTripArgs` are defined in Task 3 and registered in the same task. `Trip.kept` is added in Task 1 and read in Tasks 2, 3, 4 and 5.
