# Shared Trips, Slice 1 — People: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Several people on one trip: an owner and editors, a share link that is also an invitation, a join page on the website and in the Mini App, a People panel, and an "Added by" label on every card.

**Architecture:** `trips.account_id` keeps meaning the owner; editors are rows in a new `trip_people` table, and one SQL predicate (`REACHES`) replaces "owned by" in every lookup a trip goes through, so every existing route and desk tool works for an editor without being touched. Owner-only actions check `trips.account_id` directly. Joining is one store transaction that also gives a seat. Who added an item is two nullable columns on `trip_items`, resolved to a name per reader when the `Plan` is built.

**Tech Stack:** Rust (axum 0.8, DuckDB through `duckdb-rs`, tokio), plain JS (`chat.js`, node `--test`), the existing HTML-string pages in `pages.rs`.

**Spec:** `docs/superpowers/specs/2026-10-11-shared-trips-design.md`, Slice 1.

**Gate (run before every commit that touches code):**

```bash
cargo test --workspace 2>&1 | grep -E "^test result|FAILED|panicked" 
cargo clippy --workspace --all-targets -- -D warnings
node --test 'crates/scout-web/src/*.test.mjs'
```

Never run `cargo fmt`. Commit trailer: `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Work on branch `feat/shared-trips-people`.

## Decisions made while planning

These refine the spec; Task 0 writes them into it.

- **Leaving keeps the row.** `trip_people` gains `left_at`. A label must still name someone who left, and the name lives on their row. Rejoining clears `left_at`.
- **Sharing keeps a draft.** Making a link sets `kept`: a draft is deleted with its owner's chat, and editors would lose a trip they had just joined.
- **Deleting the owner's thread detaches a shared trip** instead of deleting it, as expiry already detaches.
- **Back to the join page after signing in** is a cookie, `__Host-scout_join`, set by the join page for an hour and read by the sign-in redirect. Only a 22-character link token is accepted from it, and the path is built from that.
- **The page already refetches trips when it becomes visible** (`chat.js`, the `visibilitychange` listener). Nothing to add.
- **No Share entry in the Mini App's own ⋯ menu.** Telegram gives a page one button there, the settings button, and it already opens the address editor. The People line is in the trip's header in the Mini App as on the website.
- **The Telegram link is always offered** (`t.me/<bot>?startapp=j_<token>`). Setting the bot's Main Mini App in BotFather is a deploy step (Task 15).
- **On the wire, an item's author** is one object, `added: {via, name, you}`, or `null`. **People** are `people: [{id, name, owner, you}]`, the reader's ownership is `you_own`, and the owner's live link is `link: {expires, admits, cap}`.

## File map

| File | What changes |
|---|---|
| `crates/scout-core/src/store.rs` | step 21; `REACHES`; reach-based trip lookups; `TripDeleted`; thread deletion detaches shared trips; `Author`/`Via`/`Added`; `Trip.owner_id`; `add_*_by`; people, links, join, leave, remove |
| `crates/scout-core/src/trips.rs` | `Plan.people/you_own/link`, `Plan::for_reader`; `Deletion` |
| `crates/scout-core/src/sharing.rs` (new) | async wrappers: link, join, preview, leave, remove, suggested name |
| `crates/scout-core/src/inbox.rs` | arrival Add credits the account, `Plan::for_reader` |
| `crates/scout-core/src/tools/trips.rs` | Scout's adds credited; delete refusal; `shared` line |
| `crates/scout-core/src/nearby.rs`, `move_check.rs` | struct literals gain new fields |
| `crates/scout-web/src/routes/trips.rs` | link, link-new, link-off, leave, remove-person; 403 on delete |
| `crates/scout-web/src/routes/join.rs` (new) | `GET/POST /join/{token}` |
| `crates/scout-web/src/routes/auth.rs` | sign-in returns to the join page |
| `crates/scout-web/src/routes/telegram_app.rs`, `tg.js`, `telegram_login.rs` | a launch with `j_<token>` opens the join page |
| `crates/scout-web/src/pages.rs` | the join pages |
| `crates/scout-web/src/chat.js`, `chat.html`, `chat.test.mjs` | People line and panel, labels, `?trip=` on the website |
| `crates/scout-web/src/trip_pdf.rs` | the label on paper |

---

### Task 0: Branch and spec amendments

**Files:** Modify `docs/superpowers/specs/2026-10-11-shared-trips-design.md`

- [ ] **Step 1: Branch**

```bash
cd /Users/watchcat/work/rust/scout && git checkout -b feat/shared-trips-people
```

- [ ] **Step 2: Amend the spec**

In the step 21 SQL, add `left_at TIMESTAMP` after `conversation_id BIGINT` in `trip_people`, and replace the bullet "`trip_people` holds editors only. A row going away is a person leaving or being removed; their items keep `added_by`." with:

```markdown
- `trip_people` holds editors only. Leaving or being removed sets
  `left_at`; the row stays, so a label can still name them. Rejoining
  clears it. Every "is on the trip" question reads `left_at IS NULL`.
```

Under "### The link", after the first paragraph, add:

```markdown
Making a link keeps the trip. A draft is deleted with its owner's chat,
and the people who joined it would lose it with no warning.
```

Under "### Joining", replace the "Not signed in" bullet's return-path sentences ("each returning to `/join/<token>` afterwards. The return path is checked …") with:

```markdown
  each returning to `/join/<token>` afterwards: the join page sets
  `__Host-scout_join` to the token for an hour, and the redirect after
  signing in reads it. Only a 22-character base64url token is accepted
  from it, and the path is built from the token, so the cookie cannot
  send anyone anywhere else. An email link opened in another browser
  lands on `/chat`, as now.
```

Under "### On the page", replace the "Refetch on focus" bullet with:

```markdown
- **Refetch on focus.** Already there: the page refetches the trips when
  it becomes visible again. Edits carry the stale-tab rule: a changed
  item is 409 and a reload.
```

and replace "Share is also in its ⋯ menu." with "Telegram's ⋯ menu has room for one button of ours, and it opens the address editor; Share is in the header."

Under "### Leaving and removing", replace "Both delete the `trip_people` row." with "Both set `left_at` on the `trip_people` row." Add a paragraph:

```markdown
Deleting the owner's thread no longer deletes a trip that has people on
it: it detaches it, as expiry does. Deleting an editor's thread clears
their `conversation_id`.
```

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-10-11-shared-trips-design.md docs/superpowers/plans/2026-10-11-shared-trips-people.md
git commit -m "docs: shared trips slice 1 plan, and what planning changed in the spec

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 1: Schema step 21

**Files:**
- Modify: `crates/scout-core/src/store.rs` (`MIGRATIONS` around lines 100–220, `STEP_20_ITEM_CHECK` at ~1333, `steps()` at ~1341, tests near `version_nineteen_db_with_an_item` at ~9020)

- [ ] **Step 1: Write the failing test**

Below `step_20_runs_on_a_database_that_has_items_in_it_and_matches_a_fresh_one`, add:

```rust
    /// A database at 20 with a trip and an item in it. Built from
    /// `MIGRATIONS` and stripped of everything step 21 adds, for the reason
    /// `version_eighteen_db_with_an_item` gives.
    fn version_twenty_db_with_an_item() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("v20.duckdb");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATIONS).unwrap();
        conn.execute_batch(
            "ALTER TABLE trips DROP COLUMN owner_name;
             ALTER TABLE trip_items DROP COLUMN added_by;
             ALTER TABLE trip_items DROP COLUMN added_via;
             DROP TABLE trip_people;
             DROP TABLE trip_links;
             CREATE TABLE IF NOT EXISTS schema_version (version BIGINT NOT NULL);
             DELETE FROM schema_version;
             INSERT INTO schema_version VALUES (20);
             INSERT INTO accounts (id) VALUES (1);
             INSERT INTO trips (id, account_id, name, name_key) VALUES (1, 1, 'Hong Kong', 'hong kong');
             INSERT INTO trip_items (id, trip_id, position, kind, title, date) VALUES (1, 1, 1, 'activity', 'Lunch', '2026-09-24');",
        )
        .unwrap();
        drop(conn);
        (dir, path)
    }

    #[test]
    fn step_21_runs_on_a_database_with_trips_in_it_and_matches_a_fresh_one() {
        let (_dir, path) = version_twenty_db_with_an_item();
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), 21);
        let trip = &s.list_trips(1).unwrap()[0];
        assert_eq!(trip.items[0].added, None, "an item from before sharing has no author");
        let (fresh, _d) = test_store();
        for table in ["trips", "trip_items", "trip_people", "trip_links"] {
            assert_eq!(shape(&fresh.conn(), table), shape(&s.conn(), table), "{table}");
        }
    }
```

(`trip.items[0].added` does not exist yet; Task 4 adds it. Until then write the assertion as a comment and restore it in Task 4, Step 1.)

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p scout-core step_21_runs 2>&1 | tail -5`
Expected: FAIL — the fixture's `DROP COLUMN owner_name` errors, since the column does not exist yet.

- [ ] **Step 3: Add the columns and tables to `MIGRATIONS`**

In `trips`, after the `kept` column and before `UNIQUE (account_id, name_key)`:

```sql
    -- What labels call the owner once the trip is shared; NULL until then.
    -- Last, so a fresh database and one migrated by step 21 agree.
    owner_name  TEXT,
```

In `trip_items`, after `checking_since    TIMESTAMP` (add a comma to that line):

```sql
    -- Who put this item on the trip and how: `page`, `scout` or `mail`.
    -- Step 21; NULL on everything older, which shows no label.
    added_by          BIGINT,
    added_via         TEXT
```

After the `item_candidates` table:

```sql
-- The editors of a trip. The owner is `trips.account_id` and is not here.
-- Leaving sets `left_at` and keeps the row, so a label can still name the
-- person; every "is on the trip" question reads `left_at IS NULL`.
-- `conversation_id` is the editor's own thread for this trip.
CREATE TABLE IF NOT EXISTS trip_people (
    trip_id         BIGINT NOT NULL,
    account_id      BIGINT NOT NULL,
    name            TEXT NOT NULL,
    joined_at       TIMESTAMP NOT NULL DEFAULT current_timestamp,
    conversation_id BIGINT,
    left_at         TIMESTAMP,
    PRIMARY KEY (trip_id, account_id)
);
-- A trip's share link. One live one per trip: making a new one revokes the
-- rest. `admits` counts the accounts it gave a seat to, at most 10.
CREATE TABLE IF NOT EXISTS trip_links (
    token      TEXT PRIMARY KEY,
    trip_id    BIGINT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    expires_at TIMESTAMP NOT NULL,
    admits     BIGINT NOT NULL DEFAULT 0,
    revoked_at TIMESTAMP
);
```

- [ ] **Step 4: Add the step**

After `STEP_20_ITEM_CHECK`:

```rust
/// Sharing: editors, links and who added what. New tables, and nullable
/// columns added in place — see `STEP_19_ITEM_COORDS` for the step that
/// tried to constrain a column on a table with rows. The tables are already
/// there on any database `MIGRATIONS` has run on, which is every one; the
/// step is the version bump and the backup that comes with it, as step 18
/// explains, and it still has to build them right.
const STEP_21_SHARING: &str = r#"
ALTER TABLE trips ADD COLUMN IF NOT EXISTS owner_name TEXT;
ALTER TABLE trip_items ADD COLUMN IF NOT EXISTS added_by BIGINT;
ALTER TABLE trip_items ADD COLUMN IF NOT EXISTS added_via TEXT;
CREATE TABLE IF NOT EXISTS trip_people (
    trip_id BIGINT NOT NULL, account_id BIGINT NOT NULL, name TEXT NOT NULL,
    joined_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    conversation_id BIGINT, left_at TIMESTAMP,
    PRIMARY KEY (trip_id, account_id)
);
CREATE TABLE IF NOT EXISTS trip_links (
    token TEXT PRIMARY KEY, trip_id BIGINT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    expires_at TIMESTAMP NOT NULL, admits BIGINT NOT NULL DEFAULT 0,
    revoked_at TIMESTAMP
);
"#;
```

and in `steps()` after `(20, …)`: `(21, Step::Sql(STEP_21_SHARING)),`

- [ ] **Step 5: Move every "latest version" assertion to 21**

```bash
sed -i '' 's/schema_version().unwrap(), 20)/schema_version().unwrap(), 21)/' crates/scout-core/src/store.rs
grep -c "schema_version().unwrap(), 21" crates/scout-core/src/store.rs
```

Expected: 12 (11 existing plus the new test). The step 19 and 20 tests asserted 20 as "latest"; they now assert 21, which is what they mean.

- [ ] **Step 6: Run the store tests**

Run: `cargo test -p scout-core store:: 2>&1 | grep -E "test result|FAILED|panicked"`
Expected: PASS. If DuckDB refuses `DROP COLUMN owner_name` because `trips` carries a `UNIQUE` constraint, build the fixture's `trips` by hand instead: `DROP TABLE trips;` then the pre-21 `CREATE TABLE trips (…)` copied from `MIGRATIONS` without `owner_name`.

- [ ] **Step 7: Commit**

```bash
git add crates/scout-core/src/store.rs
git commit -m "feat(store): step 21, people on a trip, its links, and who added what

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: Reaching a trip

**Files:** Modify `crates/scout-core/src/store.rs` (`upsert_trip` ~3299, `trip_by_id` ~3412, `find_trip` ~3424, `list_trips` ~3453, `list_kept_trips` ~3479, `keep_trip` ~3499; tests at the end of `mod tests`)

- [ ] **Step 1: Write the failing tests**

Add a test helper in `mod tests` and four tests:

```rust
    /// Puts `account` on `trip` as an editor, as joining will.
    fn put_on(s: &Store, trip_id: i64, account: i64, name: &str) {
        s.conn()
            .execute(
                "INSERT INTO trip_people (trip_id, account_id, name) VALUES (?, ?, ?)",
                params![trip_id, account, name],
            )
            .unwrap();
    }

    #[test]
    fn an_editor_finds_lists_and_reads_a_shared_trip_and_a_stranger_does_not() {
        let (s, _d) = test_store();
        let (owner, editor, stranger) = (
            s.account_for_telegram(1).unwrap(),
            s.account_for_telegram(2).unwrap(),
            s.account_for_telegram(3).unwrap(),
        );
        let trip = s.upsert_trip(owner, "Lisbon", None, None, None).unwrap();
        put_on(&s, trip.id, editor, "Masha");
        assert_eq!(s.find_trip(editor, "lisbon").unwrap().map(|t| t.id), Some(trip.id));
        assert_eq!(s.list_trips(editor).unwrap().len(), 1);
        assert!(s.trip_by_id(editor, trip.id).unwrap().is_some());
        assert!(s.find_trip(stranger, "Lisbon").unwrap().is_none());
        assert!(s.list_trips(stranger).unwrap().is_empty());
        assert!(s.trip_by_id(stranger, trip.id).unwrap().is_none());
        assert!(s.can_reach(editor, trip.id).unwrap() && !s.is_owner(editor, trip.id).unwrap());
        assert!(s.is_owner(owner, trip.id).unwrap());
    }

    #[test]
    fn someone_who_left_no_longer_reaches_the_trip() {
        let (s, _d) = test_store();
        let (owner, editor) = (s.account_for_telegram(1).unwrap(), s.account_for_telegram(2).unwrap());
        let trip = s.upsert_trip(owner, "Lisbon", None, None, None).unwrap();
        put_on(&s, trip.id, editor, "Masha");
        s.conn().execute("UPDATE trip_people SET left_at = current_timestamp", []).unwrap();
        assert!(s.find_trip(editor, "Lisbon").unwrap().is_none());
        assert!(!s.can_reach(editor, trip.id).unwrap());
    }

    #[test]
    fn an_editor_naming_a_shared_trip_edits_it_rather_than_making_another() {
        let (s, _d) = test_store();
        let (owner, editor) = (s.account_for_telegram(1).unwrap(), s.account_for_telegram(2).unwrap());
        let trip = s.upsert_trip(owner, "Lisbon", None, None, Some(10)).unwrap();
        put_on(&s, trip.id, editor, "Masha");
        let same = s.upsert_trip(editor, "LISBON", Some(3), None, Some(20)).unwrap();
        assert_eq!(same.id, trip.id);
        assert_eq!(same.adults, 3);
        assert_eq!(s.list_trips(editor).unwrap().len(), 1, "no second trip");
        // The owner's thread stays the owner's; the editor's run is theirs.
        assert_eq!(s.trip_owner(trip.id).unwrap(), Some(10));
        let theirs: Option<i64> = s
            .conn()
            .query_row("SELECT conversation_id FROM trip_people WHERE account_id = ?", params![editor], |r| r.get(0))
            .unwrap();
        assert_eq!(theirs, Some(20));
    }

    #[test]
    fn an_editor_can_keep_a_shared_draft() {
        let (s, _d) = test_store();
        let (owner, editor) = (s.account_for_telegram(1).unwrap(), s.account_for_telegram(2).unwrap());
        let trip = s.upsert_trip(owner, "Lisbon", None, None, None).unwrap();
        put_on(&s, trip.id, editor, "Masha");
        assert!(s.keep_trip(editor, "Lisbon").unwrap());
        assert!(s.find_trip(owner, "Lisbon").unwrap().unwrap().kept);
    }
```

`trip_owner` is the existing test-only read of `trips.conversation_id`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p scout-core editor 2>&1 | tail -5`
Expected: compile errors for `can_reach` / `is_owner`, then assertion failures.

- [ ] **Step 3: The predicate and the two questions**

Above `fn trip_exists`:

```rust
/// True when the account bound to the two `?` reaches the trip `t`: it owns
/// it, or it is on it and has not left. Every lookup of a trip by an
/// account goes through this, so an editor can do whatever the owner can
/// except what checks `trips.account_id` itself.
const REACHES: &str = "(t.account_id = ? OR EXISTS (SELECT 1 FROM trip_people p \
     WHERE p.trip_id = t.id AND p.account_id = ? AND p.left_at IS NULL))";

/// The trip of this name that the account reaches. Its own first, should
/// it ever reach two of one name — joining refuses that, so only a bug
/// could make the tie, and the account's own trip is the safer side of it.
fn reachable_trip_id(conn: &Connection, account_id: i64, key: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT t.id FROM trips t WHERE t.name_key = ? AND {REACHES}
                 ORDER BY (t.account_id = ?) DESC LIMIT 1"
            ),
            params![key, account_id, account_id, account_id],
            |r| r.get(0),
        )
        .optional()?)
}
```

In `impl Store`, beside `trip_by_id`:

```rust
    /// Whether this account owns the trip or is on it.
    pub fn can_reach(&self, account_id: i64, trip_id: i64) -> Result<bool> {
        let conn = self.conn();
        let n: i64 = conn.query_row(
            &format!("SELECT count(*) FROM trips t WHERE t.id = ? AND {REACHES}"),
            params![trip_id, account_id, account_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Whether this account owns the trip: what deleting it, removing
    /// people from it and turning its link off ask.
    pub fn is_owner(&self, account_id: i64, trip_id: i64) -> Result<bool> {
        let conn = self.conn();
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM trips WHERE id = ? AND account_id = ?",
            params![trip_id, account_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }
```

- [ ] **Step 4: Rewrite the lookups through it**

`trip_by_id`:

```rust
    pub fn trip_by_id(&self, account_id: i64, trip_id: i64) -> Result<Option<Trip>> {
        let conn = self.conn();
        let found: Option<i64> = conn
            .query_row(
                &format!("SELECT t.id FROM trips t WHERE t.id = ? AND {REACHES}"),
                params![trip_id, account_id, account_id],
                |row| row.get(0),
            )
            .optional()?;
        found.map(|id| load_trip(&conn, id)).transpose()
    }
```

Update its doc comment's first line to "One trip by id, if this account reaches it."

`find_trip`:

```rust
    pub fn find_trip(&self, account_id: i64, name: &str) -> Result<Option<Trip>> {
        let key = name.trim().to_lowercase();
        let conn = self.conn();
        reachable_trip_id(&conn, account_id, &key)?.map(|id| load_trip(&conn, id)).transpose()
    }
```

`list_trips` and `list_kept_trips`: replace the query with

```rust
            &format!("SELECT t.id FROM trips t WHERE {REACHES} ORDER BY t.updated_at DESC, t.id DESC")
```

(for `list_kept_trips`, `WHERE {REACHES} AND t.kept`), with `params![account_id, account_id]`, and change the doc line "Every trip this user has" to "Every trip this account reaches, its own and the ones it is on".

`keep_trip`:

```rust
    pub fn keep_trip(&self, account_id: i64, name: &str) -> Result<bool> {
        let key = name.trim().to_lowercase();
        let conn = self.conn();
        let Some(id) = reachable_trip_id(&conn, account_id, &key)? else {
            return Ok(false);
        };
        conn.execute(
            "UPDATE trips SET kept = true, updated_at = current_timestamp WHERE id = ?",
            params![id],
        )?;
        Ok(true)
    }
```

`upsert_trip`, from `let key = …` to the end:

```rust
        let key = name.to_lowercase();
        let conn = self.conn();
        // A trip the account already reaches, its own or a shared one, is
        // the trip it means: an editor saying "Lisbon" is talking about
        // the Lisbon they are on, not asking for a second.
        let found = reachable_trip_id(&conn, account_id, &key)?;
        let id: i64 = match found {
            Some(id) => id,
            None => {
                conn.execute(
                    "INSERT INTO trips (account_id, name, name_key) VALUES (?, ?, ?)
                     ON CONFLICT (account_id, name_key) DO NOTHING",
                    params![account_id, name, key],
                )?;
                conn.query_row(
                    "SELECT id FROM trips WHERE account_id = ? AND name_key = ?",
                    params![account_id, key],
                    |row| row.get(0),
                )?
            }
        };
        // Only ever fills a hole, on whichever side of the trip this
        // account is: the owner's thread on `trips`, an editor's on their
        // own row. No `updated_at` bump — adoption is bookkeeping.
        if let Some(conversation_id) = conversation_id {
            conn.execute(
                "UPDATE trips SET conversation_id = ?
                 WHERE id = ? AND account_id = ? AND conversation_id IS NULL",
                params![conversation_id, id, account_id],
            )?;
            conn.execute(
                "UPDATE trip_people SET conversation_id = ?
                 WHERE trip_id = ? AND account_id = ? AND left_at IS NULL AND conversation_id IS NULL",
                params![conversation_id, id, account_id],
            )?;
        }
        let invalidates_prices = found.is_some() && (adults.is_some() || cabin_class.is_some());
        if let Some(adults) = adults {
            conn.execute(
                "UPDATE trips SET adults = ?, updated_at = current_timestamp WHERE id = ?",
                params![adults, id],
            )?;
        }
        if let Some(cabin) = cabin_class {
            conn.execute(
                "UPDATE trips SET cabin_class = ?, updated_at = current_timestamp WHERE id = ?",
                params![cabin, id],
            )?;
        }
        if invalidates_prices {
            conn.execute(
                "UPDATE trips SET status = 'planning', updated_at = current_timestamp WHERE id = ?",
                params![id],
            )?;
        }
        load_trip(&conn, id)
```

Keep the existing comments that still apply (the separate-statements note, the price-invalidation note) above their statements.

- [ ] **Step 5: Run the store tests**

Run: `cargo test -p scout-core 2>&1 | grep -E "test result|FAILED|panicked"`
Expected: PASS, including `a_trip_is_found_by_name_case_insensitively_and_scoped_to_its_owner`.

- [ ] **Step 6: Commit**

```bash
git add crates/scout-core/src/store.rs
git commit -m "feat(store): an editor reaches a shared trip wherever its owner does

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: Only the owner deletes; a shared trip outlives the owner's thread

**Files:**
- Modify: `crates/scout-core/src/store.rs` (`delete_trip` ~4201, `delete_conversation` ~2112, `detach_trips_within` ~4366)
- Modify: `crates/scout-core/src/trips.rs` (`delete` ~202)
- Modify: `crates/scout-web/src/routes/trips.rs` (`delete_trip` ~173)
- Modify: `crates/scout-core/src/tools/trips.rs` (`DeleteTripTool::call` ~2236)

- [ ] **Step 1: Write the failing store tests**

```rust
    #[test]
    fn only_the_owner_deletes_a_shared_trip_and_its_people_and_links_go_with_it() {
        let (s, _d) = test_store();
        let (owner, editor) = (s.account_for_telegram(1).unwrap(), s.account_for_telegram(2).unwrap());
        let trip = s.upsert_trip(owner, "Lisbon", None, None, None).unwrap();
        put_on(&s, trip.id, editor, "Masha");
        s.conn().execute("UPDATE trips SET owner_name = 'Sasha' WHERE id = ?", params![trip.id]).unwrap();
        assert_eq!(s.delete_trip(editor, "Lisbon").unwrap(), TripDeleted::NotOwner { owner: Some("Sasha".into()) });
        assert!(s.find_trip(owner, "Lisbon").unwrap().is_some());
        assert_eq!(s.delete_trip(owner, "Lisbon").unwrap(), TripDeleted::Deleted);
        let left: i64 = s.conn().query_row("SELECT count(*) FROM trip_people", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 0);
        assert_eq!(s.delete_trip(owner, "Lisbon").unwrap(), TripDeleted::NotFound);
    }

    #[test]
    fn deleting_the_owners_thread_detaches_a_shared_trip_and_deletes_an_unshared_one() {
        let (s, _d) = test_store();
        let (owner, editor) = (s.account_for_telegram(1).unwrap(), s.account_for_telegram(2).unwrap());
        let thread = s.start_conversation(owner, "direct").unwrap();
        let shared = s.upsert_trip(owner, "Lisbon", None, None, Some(thread)).unwrap();
        s.upsert_trip(owner, "Porto", None, None, Some(thread)).unwrap();
        put_on(&s, shared.id, editor, "Masha");
        let theirs = s.start_conversation(editor, "direct").unwrap();
        s.upsert_trip(editor, "Lisbon", None, None, Some(theirs)).unwrap();
        assert!(s.delete_conversation(owner, thread).unwrap());
        assert!(s.find_trip(owner, "Lisbon").unwrap().is_some(), "shared: detached");
        assert!(s.find_trip(owner, "Porto").unwrap().is_none(), "not shared: gone with its chat");
        assert!(s.delete_conversation(editor, theirs).unwrap());
        let cleared: Option<i64> = s
            .conn()
            .query_row("SELECT conversation_id FROM trip_people", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cleared, None);
    }
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p scout-core shared_trip 2>&1 | tail -5` — Expected: compile error, `TripDeleted` unknown.

- [ ] **Step 3: `TripDeleted` and the owner check**

Next to `pub enum Moved`:

```rust
/// What asking to delete a trip did.
#[derive(Debug, Clone, PartialEq)]
pub enum TripDeleted {
    Deleted,
    /// No trip of that name that this account reaches — mistyped, or
    /// already gone, which is the state the caller wanted.
    NotFound,
    /// A shared trip this account is on but does not own.
    NotOwner { owner: Option<String> },
}
```

Replace `delete_trip`'s lookup:

```rust
    pub fn delete_trip(&self, account_id: i64, name: &str) -> Result<TripDeleted> {
        let key = name.trim().to_lowercase();
        let conn = self.conn();
        let Some(id) = reachable_trip_id(&conn, account_id, &key)? else {
            return Ok(TripDeleted::NotFound);
        };
        let (owner, owner_name): (i64, Option<String>) = conn.query_row(
            "SELECT account_id, owner_name FROM trips WHERE id = ?",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if owner != account_id {
            return Ok(TripDeleted::NotOwner { owner: owner_name });
        }
```

keep the deletes that follow, add before `DELETE FROM trips`:

```rust
        conn.execute("DELETE FROM trip_people WHERE trip_id = ?", params![id])?;
        conn.execute("DELETE FROM trip_links WHERE trip_id = ?", params![id])?;
```

and end with `Ok(TripDeleted::Deleted)`. Change the doc comment to "What became of the trip; see `TripDeleted`."

- [ ] **Step 4: Thread deletion**

In `delete_conversation`, right after `DELETE FROM messages …`, before the `item_candidates` delete:

```rust
            // A trip other people are on is not this thread's to take with
            // it: it is detached, as expiry detaches, and lives on.
            conn.execute(
                "UPDATE trips SET conversation_id = NULL
                 WHERE conversation_id = ?
                   AND id IN (SELECT trip_id FROM trip_people WHERE left_at IS NULL)",
                params![conversation_id],
            )?;
            // An editor's own thread for a trip, likewise forgotten.
            conn.execute(
                "UPDATE trip_people SET conversation_id = NULL WHERE conversation_id = ?",
                params![conversation_id],
            )?;
```

In `detach_trips_within`, before the existing `Ok(conn.execute(…))`:

```rust
    conn.execute(
        &format!("UPDATE trip_people SET conversation_id = NULL WHERE conversation_id IN ({holes})"),
        duckdb::params_from_iter(conversation_ids.iter()),
    )?;
```

- [ ] **Step 5: Core `delete`**

In `crates/scout-core/src/trips.rs`:

```rust
/// What `delete` did, for a page to answer with.
#[derive(Debug, Clone, PartialEq)]
pub enum Deletion {
    /// Gone; the trips the account still reaches, to repaint from.
    Deleted(Vec<Plan>),
    NotFound,
    /// Shared with this account, owned by someone else.
    NotOwner(Option<String>),
}

pub async fn delete(core: &Core, account_id: i64, name: &str) -> anyhow::Result<Deletion> {
    let store = core.store();
    let owned = name.to_string();
    match blocking(move || store.delete_trip(account_id, &owned)).await? {
        crate::store::TripDeleted::Deleted => list(core, account_id).await.map(Deletion::Deleted),
        crate::store::TripDeleted::NotFound => Ok(Deletion::NotFound),
        crate::store::TripDeleted::NotOwner { owner } => Ok(Deletion::NotOwner(owner)),
    }
}
```

Fix the tests in `trips.rs` that matched `Some(remaining)`/`None` to `Deletion::Deleted(remaining)`/`Deletion::NotFound` (`cargo test -p scout-core` names them).

- [ ] **Step 6: The route**

In `routes/trips.rs` `delete_trip`:

```rust
    match scout_core::trips::delete(&auth.core, account_id, &body.trip).await {
        Ok(scout_core::trips::Deletion::Deleted(remaining)) => axum::Json(remaining).into_response(),
        Ok(scout_core::trips::Deletion::NotFound) => StatusCode::NOT_FOUND.into_response(),
        // On the trip, so knowing it exists is no news; only its owner
        // can take it from everyone.
        Ok(scout_core::trips::Deletion::NotOwner(_)) => StatusCode::FORBIDDEN.into_response(),
        Err(e) => { /* unchanged */ }
    }
```

- [ ] **Step 7: The desk tool**

In `DeleteTripTool::call`, replace the `let deleted = …; let notes = match deleted {…}` block:

```rust
            let (deleted, notes) = match store.delete_trip(account_id, &args.trip)? {
                crate::store::TripDeleted::Deleted => (true, Vec::new()),
                crate::store::TripDeleted::NotFound => (false, vec![format!(
                    "nothing called {:?} to delete. This traveller has: {}",
                    args.trip,
                    trip_names(&store, account_id)?
                )]),
                crate::store::TripDeleted::NotOwner { owner } => (false, vec![format!(
                    "{:?} is shared with this traveller and only {} can delete it. \
                     They can leave it from the trip page instead.",
                    args.trip,
                    owner.as_deref().unwrap_or("its owner")
                )]),
            };
```

Add a test beside the existing delete-tool tests:

```rust
    #[tokio::test]
    async fn an_editor_asking_scout_to_delete_a_shared_trip_is_told_who_can() {
        let (store, _d) = crate::store::tests::test_store();
        let (owner, editor) = (store.account_for_telegram(1).unwrap(), store.account_for_telegram(2).unwrap());
        let trip = store.upsert_trip(owner, "Lisbon", None, None, None).unwrap();
        store.conn_for_tests().execute(
            "INSERT INTO trip_people (trip_id, account_id, name) VALUES (?, ?, 'Masha')",
            duckdb::params![trip.id, editor],
        ).unwrap();
        store.conn_for_tests().execute("UPDATE trips SET owner_name = 'Sasha'", []).unwrap();
        let out = DeleteTripTool { store: store.clone(), account_id: editor }
            .call(DeleteTripArgs { trip: "Lisbon".into() })
            .await
            .unwrap();
        assert!(!out.deleted);
        assert!(out.notes[0].contains("only Sasha can delete it"), "{:?}", out.notes);
    }
```

If the tool tests build their store differently, follow their local helper; `conn_for_tests` is a `#[cfg(test)] pub(crate) fn conn_for_tests(&self) -> MutexGuard<'_, Connection> { self.conn() }` to add on `Store` if nothing like it exists.

- [ ] **Step 8: Run and commit**

Run the gate. Then:

```bash
git add -A crates/
git commit -m "feat(trips): only the owner deletes a shared trip, and it outlives the owner's thread

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: Who added an item

**Files:**
- Modify: `crates/scout-core/src/store.rs` (`Trip` ~507, `TripItem` ~537, `add_item` ~3562, `add_flight`/`add_flight_checked` ~3520–3556, `add_flight_within` ~4440, `load_trip` ~4621)
- Modify: struct literals in `crates/scout-core/src/nearby.rs` (~296, ~325), `crates/scout-core/src/tools/trips.rs` (~2441, ~2476, ~5033), `crates/scout-web/src/trip_pdf.rs` (~1222, ~1289, ~1312, ~1463)
- Modify: `crates/scout-core/src/trips.rs` (`add_leg` ~297), `crates/scout-core/src/inbox.rs` (`build_item` ~1215–1275), `crates/scout-core/src/tools/trips.rs` (~1210, ~1368)

- [ ] **Step 1: Write the failing tests**

Restore the `added` assertion in Task 1's test. Add:

```rust
    #[test]
    fn an_item_remembers_who_added_it_and_how() {
        let (s, _d) = test_store();
        let owner = s.account_for_telegram(1).unwrap();
        let trip = s.upsert_trip(owner, "Lisbon", None, None, None).unwrap();
        let by = |via| Author { account_id: owner, via };
        s.add_flight_by(trip.id, "AMS", "LIS", "2026-11-02", by(Via::Page)).unwrap();
        let trip = s
            .add_item_by(trip.id, NewItem {
                kind: "activity".into(), title: "Tram 28".into(), place: None, date: "2026-11-03".into(),
                starts_at: None, ends_at: None, notes: None, booked: false, confirmation_code: None,
                price: None, currency: None, arrival_id: None,
            }, by(Via::Scout))
            .unwrap();
        let vias: Vec<_> = trip.items.iter().map(|i| i.added.as_ref().map(|a| (a.account_id, a.via.as_str()))).collect();
        assert_eq!(vias, vec![Some((owner, "page")), Some((owner, "scout"))]);
        assert_eq!(trip.owner_id, owner);
        let unattributed = s.add_flight(trip.id, "LIS", "AMS", "2026-11-09").unwrap();
        assert_eq!(unattributed.items[2].added, None);
    }
```

- [ ] **Step 2: Run it to see it fail** — `cargo test -p scout-core remembers_who 2>&1 | tail -3`; expected: compile errors.

- [ ] **Step 3: The types**

Beside `NewItem`:

```rust
/// Who put an item on a trip, and through what.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Author {
    pub account_id: i64,
    pub via: Via,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Via {
    /// Made on the trip page.
    Page,
    /// A desk tool, in a run of this account's chat.
    Scout,
    /// An arrival this account pressed Add on.
    Mail,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Via::Page => "page",
            Via::Scout => "scout",
            Via::Mail => "mail",
        }
    }
}

/// Who added an item, as it is read back. `name` and `you` are the
/// reader's to know and are filled by `Plan::for_reader`; the store leaves
/// them empty.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Added {
    #[serde(skip)]
    pub account_id: i64,
    pub via: String,
    pub name: Option<String>,
    pub you: bool,
}
```

On `Trip`, after `id`:

```rust
    /// `trips.account_id`. Not serialised, like `id`.
    #[serde(skip)]
    pub owner_id: i64,
```

On `TripItem`, after `checking`:

```rust
    /// Who put it here. `None` on everything from before sharing.
    pub added: Option<Added>,
```

- [ ] **Step 4: Read them back**

In `load_trip`, read `account_id` with the trip's other columns (`SELECT name, adults, cabin_class, status, kept, account_id …`, a sixth tuple element `row.get::<_, i64>(5)?`), add `added_by, added_via` to the item `SELECT` after the `checking_since` expression, and in the item constructor:

```rust
                added: match (r.get::<_, Option<i64>>(21)?, r.get::<_, Option<String>>(22)?) {
                    (Some(account_id), Some(via)) => Some(Added { account_id, via, name: None, you: false }),
                    _ => None,
                },
```

End with `Ok(Trip { id, owner_id, name, adults, cabin_class, status, items, kept })`.

- [ ] **Step 5: Write them**

`add_flight_within` gains a last parameter `author: Option<Author>`; its INSERT becomes:

```rust
    conn.execute(
        "INSERT INTO trip_items (trip_id, position, kind, title, origin, destination, date, added_by, added_via)
         VALUES (?, ?, 'flight', ?, ?, ?, ?, ?, ?)",
        params![
            trip_id,
            next_position(conn, trip_id)?,
            format!("{origin} → {destination}"),
            origin,
            destination,
            date,
            author.map(|a| a.account_id),
            author.map(|a| a.via.as_str())
        ],
    )?;
```

`add_flight` and `add_flight_checked` pass `None`. Add:

```rust
    /// `add_flight`, recording who added the leg.
    pub fn add_flight_by(&self, trip_id: i64, origin: &str, destination: &str, date: &str, author: Author) -> Result<Trip> {
        let conn = self.conn();
        add_flight_within(&conn, trip_id, origin, destination, date, Some(author))
    }

    /// `add_flight_checked`, recording who added the leg.
    pub fn add_flight_checked_by(&self, trip_id: i64, origin: &str, destination: &str, date: &str, author: Author) -> Result<Option<Trip>> {
        let conn = self.conn();
        if !trip_exists(&conn, trip_id)? {
            return Ok(None);
        }
        add_flight_within(&conn, trip_id, origin, destination, date, Some(author)).map(Some)
    }
```

Rename the body of `add_item` into `fn add_item_within(&self, trip_id: i64, item: NewItem, author: Option<Author>) -> Result<Trip>`, adding `added_by, added_via` to its column list, two more `?`, and `author.map(|a| a.account_id), author.map(|a| a.via.as_str())` to its params. Then:

```rust
    pub fn add_item(&self, trip_id: i64, item: NewItem) -> Result<Trip> {
        self.add_item_within(trip_id, item, None)
    }

    /// `add_item`, recording who added it.
    pub fn add_item_by(&self, trip_id: i64, item: NewItem, author: Author) -> Result<Trip> {
        self.add_item_within(trip_id, item, Some(author))
    }
```

Grep for any other `add_flight_within(` caller and pass `None`.

- [ ] **Step 6: The struct literals**

Add `added: None,` after `checking: false,` in every `TripItem { … }` literal (`grep -rn "checking: false," crates/*/src` lists them: nearby.rs, tools/trips.rs ×2, trip_pdf.rs ×3), and `owner_id: 1,` to every `Trip { … }` literal (nearby.rs:325, tools/trips.rs:~2476, trip_pdf.rs:~1312).

- [ ] **Step 7: Credit the writers**

- `crates/scout-core/src/trips.rs` `add_leg`: `store.add_flight_checked(trip.id, &origin, &destination, &date)?` → `store.add_flight_checked_by(trip.id, &origin, &destination, &date, crate::store::Author { account_id, via: crate::store::Via::Page })?`
- `crates/scout-core/src/inbox.rs` `build_item`: `store.add_flight(trip.id, origin, destination, date)?` → `store.add_flight_by(trip.id, origin, destination, date, Author { account_id, via: Via::Mail })?`, and `store.add_item(trip.id, item)?` → `store.add_item_by(trip.id, item, Author { account_id, via: Via::Mail })?` (import `crate::store::{Author, Via}`).
- `crates/scout-core/src/tools/trips.rs` add-segment tool: `.add_flight(trip.id, &origin, &destination, &date)` → `.add_flight_by(trip.id, &origin, &destination, &date, Author { account_id, via: Via::Scout })`; add-item tool: `.add_item(` → `.add_item_by(`, with `Author { account_id, via: Via::Scout }` as a third argument after the `NewItem { … }`.

Add one test to `tools/trips.rs` next to the existing add-item tool test, asserting `trip.items[0].added.as_ref().unwrap().via == "scout"` on the item the tool added.

- [ ] **Step 8: Run and commit**

Run the gate. Then:

```bash
git add -A crates/
git commit -m "feat(trips): every new item records who added it and how

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: People, links, joining, leaving

**Files:** Modify `crates/scout-core/src/store.rs` (new section after `delete_trip`; tests)

- [ ] **Step 1: Write the failing tests**

```rust
    fn at(s: &str) -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    fn joining<'a>(token: &'a str, account_id: i64, now: &str) -> Join<'a> {
        Join { token, account_id, name: "Masha", rename: None, founder: false, owner_name: "Sasha", now: at(now) }
    }

    fn shared_trip(s: &Store) -> (i64, i64) {
        let owner = s.account_for_telegram(1).unwrap();
        let trip = s.upsert_trip(owner, "Lisbon", None, None, None).unwrap();
        s.make_link(trip.id, "t".repeat(22).as_str(), at("2026-10-11 10:00")).unwrap();
        (owner, trip.id)
    }

    #[test]
    fn joining_through_a_link_puts_you_on_the_trip_and_gives_a_seat_once() {
        let (s, _d) = test_store();
        let (_owner, trip_id) = shared_trip(&s);
        let masha = s.account_for_telegram(2).unwrap();
        let token = "t".repeat(22);
        assert_eq!(
            s.join_trip(joining(&token, masha, "2026-10-12 10:00")).unwrap(),
            Joined::Joined { trip: "Lisbon".into(), seated: true }
        );
        assert!(s.is_member(masha).unwrap());
        assert!(s.can_reach(masha, trip_id).unwrap());
        assert_eq!(
            s.join_trip(joining(&token, masha, "2026-10-12 10:05")).unwrap(),
            Joined::Already { trip: "Lisbon".into() }
        );
        assert_eq!(s.live_link(trip_id, at("2026-10-12 11:00")).unwrap().unwrap().admits, 1);
        let people: Vec<_> = s.trip_people(trip_id).unwrap().into_iter().map(|p| (p.name, p.owner)).collect();
        assert_eq!(people, vec![("Sasha".to_string(), true), ("Masha".to_string(), false)]);
    }

    #[test]
    fn a_member_joining_takes_no_seat_from_the_link_and_a_revoked_one_is_refused() {
        let (s, _d) = test_store();
        let (_owner, trip_id) = shared_trip(&s);
        let token = "t".repeat(22);
        let member = s.account_for_telegram(2).unwrap();
        s.conn().execute("INSERT INTO members (account_id, code) VALUES (?, 'autumn')", params![member]).unwrap();
        assert_eq!(
            s.join_trip(joining(&token, member, "2026-10-12 10:00")).unwrap(),
            Joined::Joined { trip: "Lisbon".into(), seated: false }
        );
        assert_eq!(s.live_link(trip_id, at("2026-10-12 11:00")).unwrap().unwrap().admits, 0);
        let revoked = s.account_for_telegram(3).unwrap();
        s.conn()
            .execute("INSERT INTO members (account_id, code, revoked_at) VALUES (?, 'autumn', current_timestamp)", params![revoked])
            .unwrap();
        assert_eq!(s.join_trip(joining(&token, revoked, "2026-10-12 10:00")).unwrap(), Joined::Revoked);
        assert!(!s.can_reach(revoked, trip_id).unwrap());
    }

    #[test]
    fn a_link_that_expired_was_turned_off_or_let_in_ten_says_so() {
        let (s, _d) = test_store();
        let (owner, trip_id) = shared_trip(&s);
        let token = "t".repeat(22);
        let late = s.account_for_telegram(2).unwrap();
        assert_eq!(s.join_trip(joining(&token, late, "2026-10-25 10:01")).unwrap(), Joined::Link(LinkState::Expired));
        for n in 0..LINK_ADMITS {
            let a = s.account_for_telegram(100 + n).unwrap();
            assert!(matches!(s.join_trip(joining(&token, a, "2026-10-12 10:00")).unwrap(), Joined::Joined { seated: true, .. }));
        }
        let eleventh = s.account_for_telegram(200).unwrap();
        assert_eq!(s.join_trip(joining(&token, eleventh, "2026-10-12 10:00")).unwrap(), Joined::Full);
        assert!(!s.is_member(eleventh).unwrap(), "no seat for the eleventh");
        assert!(s.links_off(owner, trip_id, at("2026-10-12 10:00")).unwrap());
        assert_eq!(s.join_trip(joining(&token, late, "2026-10-12 10:00")).unwrap(), Joined::Link(LinkState::Off));
        assert_eq!(s.join_trip(joining("u".repeat(22).as_str(), late, "2026-10-12 10:00")).unwrap(), Joined::Link(LinkState::Unknown));
    }

    #[test]
    fn a_name_clash_is_refused_until_the_joiner_renames_their_own_trip() {
        let (s, _d) = test_store();
        let (_owner, trip_id) = shared_trip(&s);
        let token = "t".repeat(22);
        let masha = s.account_for_telegram(2).unwrap();
        s.upsert_trip(masha, "lisbon", None, None, None).unwrap();
        s.upsert_trip(masha, "Lisbon 2025", None, None, None).unwrap();
        assert_eq!(
            s.join_trip(joining(&token, masha, "2026-10-12 10:00")).unwrap(),
            Joined::Clash { name: "lisbon".into(), yours: true }
        );
        let again = Join { rename: Some("Lisbon 2025"), ..joining(&token, masha, "2026-10-12 10:00") };
        assert_eq!(s.join_trip(again).unwrap(), Joined::RenameTaken { name: "Lisbon 2025".into() });
        let renamed = Join { rename: Some("Lisbon solo"), ..joining(&token, masha, "2026-10-12 10:00") };
        assert!(matches!(s.join_trip(renamed).unwrap(), Joined::Joined { .. }));
        assert!(s.find_trip(masha, "Lisbon solo").unwrap().is_some());
        assert_eq!(s.find_trip(masha, "Lisbon").unwrap().unwrap().id, trip_id);
    }

    #[test]
    fn leaving_and_being_removed_take_the_trip_away_and_keep_the_name() {
        let (s, _d) = test_store();
        let (owner, trip_id) = shared_trip(&s);
        let token = "t".repeat(22);
        let masha = s.account_for_telegram(2).unwrap();
        s.join_trip(joining(&token, masha, "2026-10-12 10:00")).unwrap();
        assert!(!s.remove_from_trip(masha, trip_id, owner, at("2026-10-12 11:00")).unwrap(), "an editor cannot remove");
        assert!(s.leave_trip(masha, trip_id, at("2026-10-12 11:00")).unwrap());
        assert!(!s.can_reach(masha, trip_id).unwrap());
        assert_eq!(s.author_names(trip_id).unwrap().get(&masha).map(String::as_str), Some("Masha"));
        s.join_trip(joining(&token, masha, "2026-10-12 12:00")).unwrap();
        assert!(s.remove_from_trip(owner, trip_id, masha, at("2026-10-12 13:00")).unwrap());
        assert!(!s.can_reach(masha, trip_id).unwrap());
        assert!(!s.leave_trip(owner, trip_id, at("2026-10-12 13:00")).unwrap(), "the owner does not leave");
    }

    #[test]
    fn making_a_link_keeps_the_trip_and_replaces_the_last_one() {
        let (s, _d) = test_store();
        let (owner, trip_id) = shared_trip(&s);
        assert!(s.find_trip(owner, "Lisbon").unwrap().unwrap().kept);
        s.make_link(trip_id, "v".repeat(22).as_str(), at("2026-10-13 10:00")).unwrap();
        let masha = s.account_for_telegram(2).unwrap();
        assert_eq!(s.join_trip(joining(&"t".repeat(22), masha, "2026-10-13 11:00")).unwrap(), Joined::Link(LinkState::Off));
        assert_eq!(s.live_link(trip_id, at("2026-10-13 11:00")).unwrap().unwrap().token, "v".repeat(22));
    }
```

- [ ] **Step 2: Run them to see them fail** — compile errors for `Join`, `Joined`, `LinkState`, and the methods.

- [ ] **Step 3: The types**

```rust
/// How long a share link works, and how many people without a seat it
/// may let in.
pub const LINK_DAYS: i64 = 14;
pub const LINK_ADMITS: i64 = 10;

/// Someone on a trip: the owner, or an editor who has not left.
#[derive(Debug, Clone, PartialEq)]
pub struct TripPerson {
    pub account_id: i64,
    pub name: String,
    pub owner: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TripLink {
    pub token: String,
    pub trip_id: i64,
    pub expires_at: chrono::NaiveDateTime,
    pub admits: i64,
}

/// What a token is, right now.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkState {
    Live(TripLink),
    Expired,
    /// Turned off, or replaced by a newer link.
    Off,
    Unknown,
}

/// A request to join, with everything the store cannot work out itself:
/// whether the account is a founder (from the config) and what to call the
/// owner if the trip has never been shared (from their identities).
pub struct Join<'a> {
    pub token: &'a str,
    pub account_id: i64,
    pub name: &'a str,
    pub rename: Option<&'a str>,
    pub founder: bool,
    pub owner_name: &'a str,
    pub now: chrono::NaiveDateTime,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Joined {
    Joined { trip: String, seated: bool },
    Already { trip: String },
    Link(LinkState),
    Revoked,
    /// The link has given its ten seats and this account has none.
    Full,
    /// The account already reaches a trip of this name. `yours` when it is
    /// its own, which renaming can fix; otherwise it is another shared one.
    Clash { name: String, yours: bool },
    RenameTaken { name: String },
}
```

- [ ] **Step 4: The methods**

Add a free function and the `impl Store` methods:

```rust
fn link_state(conn: &Connection, token: &str, now: chrono::NaiveDateTime) -> Result<LinkState> {
    let row: Option<(i64, chrono::NaiveDateTime, i64, Option<chrono::NaiveDateTime>)> = conn
        .query_row(
            "SELECT l.trip_id, l.expires_at, l.admits, l.revoked_at
             FROM trip_links l JOIN trips t ON t.id = l.trip_id WHERE l.token = ?",
            params![token],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    Ok(match row {
        None => LinkState::Unknown,
        Some((_, _, _, Some(_))) => LinkState::Off,
        Some((_, expires_at, _, None)) if expires_at <= now => LinkState::Expired,
        Some((trip_id, expires_at, admits, None)) => {
            LinkState::Live(TripLink { token: token.to_string(), trip_id, expires_at, admits })
        }
    })
}
```

```rust
    /// The owner, then the editors in the order they joined. The owner's
    /// name is `owner_name`, or their Telegram name, or "Owner" on a trip
    /// that has never been shared.
    pub fn trip_people(&self, trip_id: i64) -> Result<Vec<TripPerson>> {
        let conn = self.conn();
        let owner: TripPerson = conn.query_row(
            "SELECT t.account_id, coalesce(t.owner_name, u.display_name, 'Owner')
             FROM trips t LEFT JOIN users u ON u.account_id = t.account_id WHERE t.id = ?",
            params![trip_id],
            |r| Ok(TripPerson { account_id: r.get(0)?, name: r.get(1)?, owner: true }),
        )?;
        let mut stmt = conn.prepare(
            "SELECT account_id, name FROM trip_people
             WHERE trip_id = ? AND left_at IS NULL ORDER BY joined_at, account_id",
        )?;
        let editors = stmt
            .query_map(params![trip_id], |r| Ok(TripPerson { account_id: r.get(0)?, name: r.get(1)?, owner: false }))?
            .collect::<duckdb::Result<Vec<_>>>()?;
        Ok(std::iter::once(owner).chain(editors).collect())
    }

    /// Every name a label on this trip may need: the owner and everyone
    /// who was ever on it, including those who left.
    pub fn author_names(&self, trip_id: i64) -> Result<std::collections::HashMap<i64, String>> {
        let mut names: std::collections::HashMap<i64, String> = self
            .trip_people(trip_id)?
            .into_iter()
            .filter(|p| p.owner)
            .map(|p| (p.account_id, p.name))
            .collect();
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT account_id, name FROM trip_people WHERE trip_id = ?")?;
        for row in stmt.query_map(params![trip_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
            let (id, name) = row?;
            names.insert(id, name);
        }
        Ok(names)
    }

    /// The thread this account talks about the trip in: the trip's own for
    /// the owner, the editor's own for an editor. `None` when there is none.
    pub fn trip_chat_for(&self, account_id: i64, trip_id: i64) -> Result<Option<TripChat>> {
        if self.is_owner(account_id, trip_id)? {
            return self.trip_chat(trip_id);
        }
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT c.id, c.title, c.scope FROM trip_people p
                 JOIN conversations c ON c.id = p.conversation_id
                 WHERE p.trip_id = ? AND p.account_id = ? AND p.left_at IS NULL",
                params![trip_id, account_id],
                |r| Ok(TripChat { id: r.get(0)?, title: r.get(1)?, scope: r.get(2)? }),
            )
            .optional()?)
    }

    /// The link a share would hand out now: not turned off, not expired,
    /// not out of seats.
    pub fn live_link(&self, trip_id: i64, now: chrono::NaiveDateTime) -> Result<Option<TripLink>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT token, trip_id, expires_at, admits FROM trip_links
                 WHERE trip_id = ? AND revoked_at IS NULL AND expires_at > ? AND admits < ?
                 ORDER BY created_at DESC LIMIT 1",
                params![trip_id, now, LINK_ADMITS],
                |r| Ok(TripLink { token: r.get(0)?, trip_id: r.get(1)?, expires_at: r.get(2)?, admits: r.get(3)? }),
            )
            .optional()?)
    }

    /// A new link, which turns off every other. Keeps the trip: a draft is
    /// deleted with its owner's chat, and people who joined it would lose
    /// it. The token comes from the caller, which has the randomness.
    pub fn make_link(&self, trip_id: i64, token: &str, now: chrono::NaiveDateTime) -> Result<TripLink> {
        let conn = self.conn();
        conn.execute(
            "UPDATE trip_links SET revoked_at = ? WHERE trip_id = ? AND revoked_at IS NULL",
            params![now, trip_id],
        )?;
        let expires_at = now + chrono::Duration::days(LINK_DAYS);
        conn.execute(
            "INSERT INTO trip_links (token, trip_id, created_at, expires_at) VALUES (?, ?, ?, ?)",
            params![token, trip_id, now, expires_at],
        )?;
        conn.execute("UPDATE trips SET kept = true WHERE id = ?", params![trip_id])?;
        Ok(TripLink { token: token.to_string(), trip_id, expires_at, admits: 0 })
    }

    /// Turns the trip's links off. False when this account is not the owner.
    pub fn links_off(&self, account_id: i64, trip_id: i64, now: chrono::NaiveDateTime) -> Result<bool> {
        if !self.is_owner(account_id, trip_id)? {
            return Ok(false);
        }
        let conn = self.conn();
        conn.execute(
            "UPDATE trip_links SET revoked_at = ? WHERE trip_id = ? AND revoked_at IS NULL",
            params![now, trip_id],
        )?;
        Ok(true)
    }

    /// What a token opens, for the join page before anyone signs in.
    pub fn link_trip(&self, token: &str, now: chrono::NaiveDateTime) -> Result<(LinkState, Option<Trip>)> {
        let conn = self.conn();
        let trip_id: Option<i64> = conn
            .query_row("SELECT trip_id FROM trip_links WHERE token = ?", params![token], |r| r.get(0))
            .optional()?;
        let state = link_state(&conn, token, now)?;
        let trip = match trip_id {
            Some(id) if trip_exists(&conn, id)? => Some(load_trip(&conn, id)?),
            _ => None,
        };
        Ok((state, trip))
    }

    /// Joining, in one transaction: see the spec's "Joining" for the order.
    pub fn join_trip(&self, j: Join<'_>) -> Result<Joined> {
        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        let result = join_within(&conn, &j);
        match result {
            Ok(joined) => {
                conn.execute_batch("COMMIT")?;
                Ok(joined)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// An editor leaving. False when they were not on it — or are its
    /// owner, who deletes rather than leaves.
    pub fn leave_trip(&self, account_id: i64, trip_id: i64, now: chrono::NaiveDateTime) -> Result<bool> {
        let conn = self.conn();
        Ok(conn.execute(
            "UPDATE trip_people SET left_at = ?, conversation_id = NULL
             WHERE trip_id = ? AND account_id = ? AND left_at IS NULL",
            params![now, trip_id, account_id],
        )? > 0)
    }

    /// The owner taking someone off. False for anyone but the owner.
    pub fn remove_from_trip(&self, owner: i64, trip_id: i64, person: i64, now: chrono::NaiveDateTime) -> Result<bool> {
        if !self.is_owner(owner, trip_id)? {
            return Ok(false);
        }
        self.leave_trip(person, trip_id, now)
    }
```

and the transaction body, a free function:

```rust
fn join_within(conn: &Connection, j: &Join<'_>) -> Result<Joined> {
    let link = match link_state(conn, j.token, j.now)? {
        LinkState::Live(link) => link,
        other => return Ok(Joined::Link(other)),
    };
    let (owner, name, key): (i64, String, String) = conn.query_row(
        "SELECT account_id, name, name_key FROM trips WHERE id = ?",
        params![link.trip_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let on_it: i64 = conn.query_row(
        "SELECT count(*) FROM trip_people WHERE trip_id = ? AND account_id = ? AND left_at IS NULL",
        params![link.trip_id, j.account_id],
        |r| r.get(0),
    )?;
    if owner == j.account_id || on_it > 0 {
        return Ok(Joined::Already { trip: name });
    }
    let standing: Option<bool> = conn
        .query_row("SELECT revoked_at IS NULL FROM members WHERE account_id = ?", params![j.account_id], |r| r.get(0))
        .optional()?;
    if standing == Some(false) {
        return Ok(Joined::Revoked);
    }
    if let Some(new) = j.rename.map(str::trim).filter(|n| !n.is_empty()) {
        let new_key = new.to_lowercase();
        if reachable_trip_id(conn, j.account_id, &new_key)?.is_some() {
            return Ok(Joined::RenameTaken { name: new.to_string() });
        }
        conn.execute(
            "UPDATE trips SET name = ?, name_key = ?, updated_at = current_timestamp
             WHERE account_id = ? AND name_key = ?",
            params![new, new_key, j.account_id, key],
        )?;
    }
    if let Some(clash) = reachable_trip_id(conn, j.account_id, &key)? {
        let (clash_name, clash_owner): (String, i64) = conn.query_row(
            "SELECT name, account_id FROM trips WHERE id = ?",
            params![clash],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        return Ok(Joined::Clash { name: clash_name, yours: clash_owner == j.account_id });
    }
    let seated_already = standing == Some(true) || j.founder;
    if !seated_already {
        if link.admits >= LINK_ADMITS {
            return Ok(Joined::Full);
        }
        conn.execute(
            "INSERT INTO members (account_id, code) VALUES (?, ?)",
            params![j.account_id, format!("trip-{}", link.trip_id)],
        )?;
        conn.execute("DELETE FROM waitlist WHERE account_id = ?", params![j.account_id])?;
        conn.execute("UPDATE trip_links SET admits = admits + 1 WHERE token = ?", params![j.token])?;
    }
    let returning = conn.execute(
        "UPDATE trip_people SET left_at = NULL, name = ?, joined_at = ?
         WHERE trip_id = ? AND account_id = ?",
        params![j.name, j.now, link.trip_id, j.account_id],
    )?;
    if returning == 0 {
        conn.execute(
            "INSERT INTO trip_people (trip_id, account_id, name, joined_at) VALUES (?, ?, ?, ?)",
            params![link.trip_id, j.account_id, j.name, j.now],
        )?;
    }
    conn.execute(
        "UPDATE trips SET owner_name = ? WHERE id = ? AND owner_name IS NULL",
        params![j.owner_name, link.trip_id],
    )?;
    Ok(Joined::Joined { trip: name, seated: !seated_already })
}
```

The rename runs before the clash check, as the spec orders it, and is undone by the transaction's rollback only on an error — a `RenameTaken` or `Clash` return commits nothing because nothing was written before it. If DuckDB refuses the `UPDATE … name_key` (a `UNIQUE` column), note the error in the commit and rebuild the row instead: `INSERT` a copy under the new name, move its `trip_items.trip_id`, `DELETE` the old row — inside the same transaction.

- [ ] **Step 5: Run and commit**

Run: `cargo test -p scout-core store:: 2>&1 | grep -E "test result|FAILED|panicked"` → PASS. Gate. Then:

```bash
git add crates/scout-core/src/store.rs
git commit -m "feat(store): share links, joining with a seat, leaving and removing

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 6: A plan as its reader sees it

**Files:**
- Modify: `crates/scout-core/src/trips.rs` (`Plan` ~22, `from_trip` ~59, every `trip_chat` + `from_trip` pair)
- Modify: `crates/scout-core/src/inbox.rs` (~1153)
- Modify: `crates/scout-core/src/nearby.rs` (~323, the `Plan { … }` literal)

- [ ] **Step 1: Write the failing test** (in `trips.rs` `mod tests`)

```rust
    #[tokio::test]
    async fn a_shared_trip_names_its_people_and_its_authors_for_each_reader() {
        let (core, _dir, owner) = core().await;
        seed_trip_for_tests(&core, owner, "October").await.unwrap();
        let masha = core.store().account_for_telegram(12).unwrap();
        crate::sharing::share_for_tests(&core, owner, "October", masha, "Masha").await.unwrap();
        let store = core.store();
        let trip = store.find_trip(masha, "October").unwrap().unwrap();
        store
            .add_item_by(trip.id, crate::store::NewItem {
                kind: "activity".into(), title: "Fado".into(), place: None, date: "2026-10-03".into(),
                starts_at: None, ends_at: None, notes: None, booked: false, confirmation_code: None,
                price: None, currency: None, arrival_id: None,
            }, crate::store::Author { account_id: masha, via: crate::store::Via::Scout })
            .unwrap();

        let theirs = find(&core, masha, "October").await.unwrap().unwrap();
        assert!(!theirs.you_own);
        let people: Vec<_> = theirs.people.iter().map(|p| (p.name.as_str(), p.owner, p.you)).collect();
        assert_eq!(people, vec![("Sasha", true, false), ("Masha", false, true)]);
        let fado = theirs.trip.items.iter().find(|i| i.title == "Fado").unwrap();
        assert!(fado.added.as_ref().unwrap().you);
        assert!(theirs.link.is_none(), "only the owner sees the link's state");

        let mine = find(&core, owner, "October").await.unwrap().unwrap();
        assert!(mine.you_own);
        let fado = mine.trip.items.iter().find(|i| i.title == "Fado").unwrap();
        let added = fado.added.as_ref().unwrap();
        assert_eq!((added.name.as_deref(), added.you, added.via.as_str()), (Some("Masha"), false, "scout"));
        assert!(mine.link.is_some());
    }
```

(`share_for_tests` is Task 7's; write Task 7's Step 3 first if executing strictly in order, or let this test fail to compile until Task 7 lands and commit Tasks 6–7 together.)

- [ ] **Step 2: The fields and `for_reader`**

On `Plan`, after `chat`:

```rust
    /// Who is on the trip, the owner first. One entry on a trip of one.
    pub people: Vec<Person>,
    /// Whether the reader owns the trip: what the page shows Remove,
    /// the link's controls and Delete to.
    pub you_own: bool,
    /// The share link's state, for the owner, while one works.
    pub link: Option<LinkView>,
```

and the two wire types:

```rust
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Person {
    /// The account id, which the owner's Remove sends back.
    pub id: i64,
    pub name: String,
    pub owner: bool,
    pub you: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct LinkView {
    /// `YYYY-MM-DD`, the last day it works.
    pub expires: String,
    pub admits: i64,
    pub cap: i64,
}

impl LinkView {
    pub(crate) fn of(link: &crate::store::TripLink) -> Self {
        Self {
            expires: (link.expires_at - chrono::Duration::seconds(1)).date().to_string(),
            admits: link.admits,
            cap: crate::store::LINK_ADMITS,
        }
    }
}
```

`from_trip` sets `people: Vec::new(), you_own: true, link: None`. Add:

```rust
    /// The plan as this account sees it: its own thread for the trip, who
    /// is on it, and every item's author named, "you" for the reader.
    pub(crate) fn for_reader(store: &crate::store::Store, account_id: i64, trip: Trip) -> anyhow::Result<Self> {
        let chat = store.trip_chat_for(account_id, trip.id)?;
        let people = store.trip_people(trip.id)?;
        let names = store.author_names(trip.id)?;
        let link = if trip.owner_id == account_id {
            store.live_link(trip.id, crate::sharing::now())?
        } else {
            None
        };
        let mut plan = Self::from_trip(trip, chat);
        plan.you_own = plan.trip.owner_id == account_id;
        plan.people = people
            .into_iter()
            .map(|p| Person { you: p.account_id == account_id, id: p.account_id, name: p.name, owner: p.owner })
            .collect();
        for item in &mut plan.trip.items {
            if let Some(added) = item.added.as_mut() {
                added.you = added.account_id == account_id;
                added.name = names.get(&added.account_id).cloned();
            }
        }
        plan.link = link.as_ref().map(LinkView::of);
        Ok(plan)
    }
```

- [ ] **Step 3: Build every plan through it**

In `trips.rs` and `inbox.rs`, replace each

```rust
let chat = store.trip_chat(trip.id)?;
… Plan::from_trip(trip, chat) …
```

with `Plan::for_reader(&store, account_id, trip)?` in the same position (inside `Box::new(…)`, `Some(…)` or `Selection::Chosen(…)` as before). `seed_trip_for_tests`' `Plan::from_trip(trip, None)` becomes `Plan::for_reader(&store, account_id, trip)?`. After the edit, `grep -n "trip_chat(trip.id)" crates/scout-core/src/trips.rs crates/scout-core/src/inbox.rs` should print nothing.

In `nearby.rs` `plan()`, add `people: vec![], you_own: true, link: None,` to the literal.

- [ ] **Step 4: Run and commit** (with Task 7 if the test needs `share_for_tests`)

---

### Task 7: The sharing module

**Files:** Create `crates/scout-core/src/sharing.rs`; modify `crates/scout-core/src/lib.rs` (`pub mod sharing;` after `pub mod session;`), `crates/scout-core/src/store.rs` (a `display_name_of` read)

- [ ] **Step 1: Write the failing tests** (at the bottom of `sharing.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    async fn core() -> (Core, tempfile::TempDir, i64) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sharing.duckdb");
        let core = Core::start(crate::config::Config::for_test(path.to_str().unwrap()), None).unwrap();
        let owner = core.store().account_for_telegram(11).unwrap();
        crate::trips::seed_trip_for_tests(&core, owner, "October").await.unwrap();
        (core, dir, owner)
    }

    #[tokio::test]
    async fn the_owner_shares_someone_joins_and_the_owner_takes_them_off() {
        let (core, _dir, owner) = core().await;
        let link = link(&core, owner, "October", "a".repeat(22)).await.unwrap().unwrap();
        assert_eq!(link.token, "a".repeat(22));
        let again = link(&core, owner, "October", "b".repeat(22)).await.unwrap();
        assert_eq!(again.unwrap().token, "a".repeat(22), "the live link, not a new one");

        let masha = core.store().account_for_telegram(12).unwrap();
        let preview = preview(&core, &link.token).await.unwrap().unwrap();
        assert_eq!(preview.trip, "October");
        assert!(matches!(
            join(&core, masha, &link.token, "Masha", None).await.unwrap(),
            Joined::Joined { seated: true, .. }
        ));
        assert_eq!(
            remove(&core, masha, "October", owner).await.unwrap(),
            Owned::NotOwner
        );
        assert_eq!(remove(&core, owner, "October", masha).await.unwrap(), Owned::Done(()));
        assert!(crate::trips::find(&core, masha, "October").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_suggested_name_is_the_first_word_of_a_telegram_name_then_an_email() {
        let (core, _dir, owner) = core().await;
        core.store().conn_for_tests().execute(
            "INSERT INTO users (account_id, display_name) VALUES (?, 'Sasha Petrova')",
            duckdb::params![owner],
        ).unwrap();
        assert_eq!(suggested_name(&core, owner).await.unwrap(), "Sasha");
        let web = core.store().account_for_telegram(99).unwrap();
        assert_eq!(suggested_name(&core, web).await.unwrap(), "Traveller");
    }
}
```

- [ ] **Step 2: The module**

```rust
//! Sharing a trip: its link, joining it, leaving it, and taking someone
//! off it. Account-facing and async, as `trips` is; the rules live in the
//! store, under its lock.

use crate::core::{blocking, Core};
pub use crate::store::{Joined, LinkState, TripLink, LINK_ADMITS, LINK_DAYS};

/// UTC, naive: the clock every timestamp in `trip_links` is written and
/// compared in.
pub fn now() -> chrono::NaiveDateTime {
    chrono::Utc::now().naive_utc()
}

/// The answer to an owner-only action.
#[derive(Debug, Clone, PartialEq)]
pub enum Owned<T> {
    Done(T),
    /// No trip of that name the account reaches.
    NotFound,
    /// On it, but not its owner.
    NotOwner,
}

/// What the join page shows before anyone signs in.
#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    pub state: LinkState,
    pub trip: String,
    pub first: Option<String>,
    pub last: Option<String>,
    pub people: Vec<String>,
}

fn trip_id(store: &crate::store::Store, account_id: i64, name: &str) -> anyhow::Result<Option<i64>> {
    Ok(store.find_trip(account_id, name)?.map(|t| t.id))
}

/// The trip's live link, made from `fresh` if there is none. Anyone on
/// the trip may ask; `None` when the account does not reach it.
pub async fn link(core: &Core, account_id: i64, trip: &str, fresh: String) -> anyhow::Result<Option<TripLink>> {
    let store = core.store();
    let trip = trip.to_string();
    blocking(move || {
        let Some(id) = trip_id(&store, account_id, &trip)? else { return Ok(None) };
        match store.live_link(id, now())? {
            Some(live) => Ok(Some(live)),
            None => store.make_link(id, &fresh, now()).map(Some),
        }
    })
    .await
}

/// A new link in place of the old, for the owner.
pub async fn new_link(core: &Core, account_id: i64, trip: &str, fresh: String) -> anyhow::Result<Owned<TripLink>> {
    let store = core.store();
    let trip = trip.to_string();
    blocking(move || {
        let Some(id) = trip_id(&store, account_id, &trip)? else { return Ok(Owned::NotFound) };
        if !store.is_owner(account_id, id)? {
            return Ok(Owned::NotOwner);
        }
        store.make_link(id, &fresh, now()).map(Owned::Done)
    })
    .await
}

pub async fn link_off(core: &Core, account_id: i64, trip: &str) -> anyhow::Result<Owned<()>> {
    let store = core.store();
    let trip = trip.to_string();
    blocking(move || {
        let Some(id) = trip_id(&store, account_id, &trip)? else { return Ok(Owned::NotFound) };
        Ok(if store.links_off(account_id, id, now())? { Owned::Done(()) } else { Owned::NotOwner })
    })
    .await
}

/// An editor leaving. `NotOwner` is the owner asking, who cannot leave.
pub async fn leave(core: &Core, account_id: i64, trip: &str) -> anyhow::Result<Owned<()>> {
    let store = core.store();
    let trip = trip.to_string();
    blocking(move || {
        let Some(id) = trip_id(&store, account_id, &trip)? else { return Ok(Owned::NotFound) };
        Ok(if store.leave_trip(account_id, id, now())? { Owned::Done(()) } else { Owned::NotOwner })
    })
    .await
}

pub async fn remove(core: &Core, account_id: i64, trip: &str, person: i64) -> anyhow::Result<Owned<()>> {
    let store = core.store();
    let trip = trip.to_string();
    blocking(move || {
        let Some(id) = trip_id(&store, account_id, &trip)? else { return Ok(Owned::NotFound) };
        if !store.is_owner(account_id, id)? {
            return Ok(Owned::NotOwner);
        }
        store.remove_from_trip(account_id, id, person, now())?;
        Ok(Owned::Done(()))
    })
    .await
}

/// `None` for a token that was never made.
pub async fn preview(core: &Core, token: &str) -> anyhow::Result<Option<Preview>> {
    let store = core.store();
    let token = token.to_string();
    blocking(move || {
        let (state, trip) = store.link_trip(&token, now())?;
        let Some(trip) = trip else { return Ok(None) };
        let people = store.trip_people(trip.id)?.into_iter().map(|p| p.name).collect();
        Ok(Some(Preview {
            state,
            first: trip.items.first().map(|i| i.date.clone()),
            last: trip.items.iter().map(|i| i.ends_at.as_deref().map(|e| e[..10].to_string()).unwrap_or_else(|| i.date.clone())).max(),
            trip: trip.name,
            people,
        }))
    })
    .await
}

/// What to call someone on a trip until they say: the first word of their
/// Telegram name, else what comes before the @ of their oldest address,
/// else "Traveller". Cut to 40 characters.
pub async fn suggested_name(core: &Core, account_id: i64) -> anyhow::Result<String> {
    let store = core.store();
    blocking(move || {
        let telegram = store.display_name_of(account_id)?.and_then(|n| n.split_whitespace().next().map(str::to_string));
        let email = || -> anyhow::Result<Option<String>> {
            Ok(store.emails_of(account_id)?.first().and_then(|e| e.split('@').next().map(str::to_string)))
        };
        let name = match telegram {
            Some(name) => name,
            None => email()?.unwrap_or_else(|| "Traveller".to_string()),
        };
        Ok(name.chars().take(40).collect())
    })
    .await
}

/// Joins, and tells the Telegram gate when that gave a seat.
pub async fn join(core: &Core, account_id: i64, token: &str, name: &str, rename: Option<&str>) -> anyhow::Result<Joined> {
    let founder = core.founder_account(account_id).await?;
    let owner = {
        let store = core.store();
        let token = token.to_string();
        blocking(move || Ok(store.link_trip(&token, now())?.1.map(|t| t.owner_id))).await?
    };
    let owner_name = match owner {
        Some(owner) => suggested_name(core, owner).await?,
        None => "Owner".to_string(),
    };
    let name: String = name.trim().chars().take(40).collect();
    let name = if name.is_empty() { suggested_name(core, account_id).await? } else { name };
    let store = core.store();
    let (token, rename) = (token.to_string(), rename.map(str::to_string));
    let joined = blocking(move || {
        store.join_trip(crate::store::Join {
            token: &token,
            account_id,
            name: &name,
            rename: rename.as_deref(),
            founder,
            owner_name: &owner_name,
            now: now(),
        })
    })
    .await?;
    if matches!(joined, Joined::Joined { seated: true, .. }) {
        core.note_membership_changed();
    }
    Ok(joined)
}

/// Puts `account` on the owner's trip as joining would, with a seat-free
/// link. Test scaffolding for the other crates.
#[doc(hidden)]
pub async fn share_for_tests(core: &Core, owner: i64, trip: &str, account: i64, name: &str) -> anyhow::Result<()> {
    let token = format!("{:0>22}", account);
    link(core, owner, trip, token.clone()).await?;
    let link = link(core, owner, trip, token).await?.expect("the owner's trip");
    match join(core, account, &link.token, name, None).await? {
        Joined::Joined { .. } | Joined::Already { .. } => Ok(()),
        other => anyhow::bail!("could not share for a test: {other:?}"),
    }
}
```

`share_for_tests` and `join` use `founder_account`, which reads the config's founder list; the test config has none, so the joiner is seated by the link — fine for tests.

In the store, beside `display_names`:

```rust
    /// The latest name Telegram gave this account, if it ever wrote to the bot.
    pub fn display_name_of(&self, account_id: i64) -> Result<Option<String>> {
        let conn = self.conn();
        Ok(conn
            .query_row("SELECT display_name FROM users WHERE account_id = ?", params![account_id], |r| r.get(0))
            .optional()?)
    }
```

- [ ] **Step 3: Run and commit Tasks 6 and 7**

Gate. Then:

```bash
git add -A crates/scout-core
git commit -m "feat(trips): a plan names its people and authors for whoever reads it

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 8: What Scout sees of a shared trip

**Files:** Modify `crates/scout-core/src/tools/trips.rs` (`TripView` ~144, `TripView::of` ~178, `ShowTripTool::call` ~1733)

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn show_trip_tells_scout_who_the_trip_is_shared_with_and_which_one_is_asking() {
        let (store, _d) = crate::store::tests::test_store();
        let (owner, editor) = (store.account_for_telegram(1).unwrap(), store.account_for_telegram(2).unwrap());
        let trip = store.upsert_trip(owner, "Lisbon", None, None, None).unwrap();
        store.conn_for_tests().execute(
            "INSERT INTO trip_people (trip_id, account_id, name) VALUES (?, ?, 'Masha')",
            duckdb::params![trip.id, editor],
        ).unwrap();
        store.conn_for_tests().execute("UPDATE trips SET owner_name = 'Sasha'", []).unwrap();
        let out = ShowTripTool { store: store.clone(), account_id: editor }
            .call(ShowTripArgs { trip: Some("Lisbon".into()) })
            .await
            .unwrap();
        assert_eq!(out.trips[0].shared.as_deref(), Some("Shared: Sasha (owner), Masha. You are an editor."));
        let solo = store.upsert_trip(owner, "Porto", None, None, None).unwrap();
        let out = ShowTripTool { store, account_id: owner }.call(ShowTripArgs { trip: Some(solo.name) }).await.unwrap();
        assert_eq!(out.trips[0].shared, None);
    }
```

- [ ] **Step 2: Implement**

On `TripView`, after `notes`:

```rust
    /// Who else is on this trip and whether the asker owns it. Absent on a
    /// trip of one, so a solo trip reads exactly as before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared: Option<String>,
```

`of` sets `shared: None`. Add:

```rust
fn shared_line(store: &Store, account_id: i64, trip: &Trip) -> anyhow::Result<Option<String>> {
    let people = store.trip_people(trip.id)?;
    if people.len() < 2 {
        return Ok(None);
    }
    let names = people
        .iter()
        .map(|p| if p.owner { format!("{} (owner)", p.name) } else { p.name.clone() })
        .collect::<Vec<_>>()
        .join(", ");
    let role = if trip.owner_id == account_id { "You are the owner." } else { "You are an editor." };
    Ok(Some(format!("Shared: {names}. {role}")))
}
```

In `ShowTripTool::call`, have the blocking closure return `Vec<TripView>`:

```rust
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<TripView>> {
            let trips = match &args.trip {
                None => store.list_trips(account_id)?,
                Some(name) => vec![find_trip_or_list(&store, account_id, name)?],
            };
            trips
                .into_iter()
                .map(|trip| {
                    let shared = shared_line(&store, account_id, &trip)?;
                    Ok(TripView { shared, ..TripView::of(trip) })
                })
                .collect()
        })
        .await
        .map_err(internal)?
        .map(|trips| TripList { trips })
        .map_err(internal)
```

- [ ] **Step 3: Run and commit**

Gate. Commit: `feat(desk): Scout is told who a trip is shared with`.

---

### Task 9: Share, leave and remove routes

**Files:** Modify `crates/scout-web/src/routes/trips.rs`

- [ ] **Step 1: Write the failing tests** (in `mod tests`)

```rust
    /// A second admitted account on the same app, with its cookie and token.
    async fn second(core: &scout_core::core::Core) -> (i64, String, String) {
        let scout_core::identity::SignIn::In { account_id } =
            scout_core::identity::sign_in(core, "telegram", "778").await.unwrap()
        else {
            panic!("the open round should admit the account");
        };
        (account_id, crate::session::mint(TEST_KEY, account_id, DAY), crate::session::csrf_for(TEST_KEY, account_id))
    }

    #[tokio::test]
    async fn anyone_on_a_trip_gets_its_link_and_only_the_owner_turns_it_off() {
        let (app, core, _dir, owner, cookie, csrf) = setup().await;
        let (masha, m_cookie, m_csrf) = second(&core).await;
        let res = post_json(&app, "/chat/trips/link", &cookie, Some(&csrf), r#"{"trip":"October"}"#).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        let url = body["url"].as_str().unwrap();
        assert!(url.ends_with(&format!("/join/{}", body["token"].as_str().unwrap())), "{url}");
        assert_eq!(body["token"].as_str().unwrap().len(), 22);
        scout_core::sharing::share_for_tests(&core, owner, "October", masha, "Masha").await.unwrap();
        let res = post_json(&app, "/chat/trips/link", &m_cookie, Some(&m_csrf), r#"{"trip":"October"}"#).await;
        assert_eq!(res.status(), StatusCode::OK, "an editor can copy the link");
        for path in ["/chat/trips/link-off", "/chat/trips/link-new"] {
            let res = post_json(&app, path, &m_cookie, Some(&m_csrf), r#"{"trip":"October"}"#).await;
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{path}");
        }
        let res = post_json(&app, "/chat/trips/link-off", &cookie, Some(&csrf), r#"{"trip":"October"}"#).await;
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn an_editor_leaves_the_owner_removes_and_an_editor_cannot_delete() {
        let (app, core, _dir, owner, cookie, csrf) = setup().await;
        let (masha, m_cookie, m_csrf) = second(&core).await;
        scout_core::sharing::share_for_tests(&core, owner, "October", masha, "Masha").await.unwrap();
        let res = delete_json(&app, "/chat/trips", &m_cookie, Some(&m_csrf), r#"{"trip":"October"}"#).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let res = post_json(&app, "/chat/trips/leave", &cookie, Some(&csrf), r#"{"trip":"October"}"#).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "the owner deletes, not leaves");
        let res = post_json(&app, "/chat/trips/leave", &m_cookie, Some(&m_csrf), r#"{"trip":"October"}"#).await;
        assert_eq!(res.status(), StatusCode::OK);
        let listed: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_eq!(listed.as_array().unwrap().len(), 0, "answers with what the leaver still has");
        scout_core::sharing::share_for_tests(&core, owner, "October", masha, "Masha").await.unwrap();
        let body = format!(r#"{{"trip":"October","person":{masha}}}"#);
        let res = post_json(&app, "/chat/trips/remove-person", &m_cookie, Some(&m_csrf), &body).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let res = post_json(&app, "/chat/trips/remove-person", &cookie, Some(&csrf), &body).await;
        assert_eq!(res.status(), StatusCode::OK);
        let res = get_with_cookie(&app, "/chat/trips", &m_cookie).await;
        let listed: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert!(listed.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_trip_list_carries_people_ownership_and_authors() {
        let (app, core, _dir, owner, cookie, _csrf) = setup().await;
        let (masha, _, _) = second(&core).await;
        scout_core::sharing::share_for_tests(&core, owner, "October", masha, "Masha").await.unwrap();
        let res = get_with_cookie(&app, "/chat/trips", &cookie).await;
        let body: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_eq!(body[0]["you_own"], true);
        assert_eq!(body[0]["people"][1]["name"], "Masha");
        assert!(body[0]["link"]["expires"].is_string());
        assert!(body[0]["items"][0].get("added").is_some());
    }
```

- [ ] **Step 2: Implement**

Routes, after `item-warning`:

```rust
        .route("/chat/trips/link", post(share_link))
        .route("/chat/trips/link-new", post(new_link))
        .route("/chat/trips/link-off", post(link_off))
        .route("/chat/trips/leave", post(leave))
        .route("/chat/trips/remove-person", post(remove_person))
```

Handlers:

```rust
/// 16 random bytes, base64url: a share link's token. 22 characters, the
/// shape `/join` and the sign-in redirect check for.
pub(crate) fn fresh_link_token() -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<[u8; 16]>())
}

/// Whether a string is a link token's shape and nothing else.
pub(crate) fn is_link_token(s: &str) -> bool {
    s.len() == 22 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// What the page needs to share a link: the web address, the Telegram one
/// that opens the Mini App, and the link's state.
fn link_json(auth: &AuthState, link: &scout_core::sharing::TripLink) -> serde_json::Value {
    let base = auth.cfg.base_url.trim_end_matches('/');
    let bot = auth.core.return_url().and_then(|u| u.rsplit('/').next().map(str::to_string)).filter(|u| !u.is_empty());
    let view = scout_core::trips::LinkView::of(link);
    serde_json::json!({
        "token": link.token,
        "url": format!("{base}/join/{}", link.token),
        "telegram_url": bot.map(|bot| format!("https://t.me/{bot}?startapp=j_{}", link.token)),
        "expires": view.expires,
        "admits": view.admits,
        "cap": view.cap,
    })
}

#[derive(serde::Deserialize)]
struct TripIn {
    trip: String,
}

async fn share_link(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<TripIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match scout_core::sharing::link(&auth.core, account_id, &body.trip, fresh_link_token()).await {
        Ok(Some(link)) => axum::Json(link_json(&auth, &link)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not make a share link");
            sorry()
        }
    }
}

/// An owner-only answer as a response: 404 for a trip the account does
/// not reach, 403 for one it is on but does not own.
fn owned<T>(out: anyhow::Result<scout_core::sharing::Owned<T>>, account_id: i64, what: &str, done: impl FnOnce(T) -> Response) -> Response {
    use scout_core::sharing::Owned;
    match out {
        Ok(Owned::Done(value)) => done(value),
        Ok(Owned::NotFound) => StatusCode::NOT_FOUND.into_response(),
        Ok(Owned::NotOwner) => StatusCode::FORBIDDEN.into_response(),
        Err(e) => {
            tracing::error!(error = %e, account_id, what, "a sharing action failed");
            sorry()
        }
    }
}
```

`new_link`, `link_off`, `leave` follow `share_link`'s shape with the admission and form-token lines unchanged, then:

```rust
    // new_link
    let out = scout_core::sharing::new_link(&auth.core, account_id, &body.trip, fresh_link_token()).await;
    owned(out, account_id, "new link", |link| axum::Json(link_json(&auth, &link)).into_response())

    // link_off
    let out = scout_core::sharing::link_off(&auth.core, account_id, &body.trip).await;
    owned(out, account_id, "link off", |()| StatusCode::OK.into_response())
```

`leave` and `remove_person` answer with the account's trips, as delete does:

```rust
    // leave
    let out = scout_core::sharing::leave(&auth.core, account_id, &body.trip).await;
    match out {
        Ok(scout_core::sharing::Owned::Done(())) => list(axum::extract::State(auth), headers).await,
        other => owned(other, account_id, "leave", |()| unreachable!()),
    }
```

```rust
#[derive(serde::Deserialize)]
struct RemovePersonIn {
    trip: String,
    person: i64,
}

    // remove_person
    let out = scout_core::sharing::remove(&auth.core, account_id, &body.trip, body.person).await;
    match out {
        Ok(scout_core::sharing::Owned::Done(())) => list(axum::extract::State(auth), headers).await,
        other => owned(other, account_id, "remove person", |()| unreachable!()),
    }
```

(`owned`'s `done` closure is never called for `Done` in these two, which `match` has already taken; `unreachable!()` documents that. If clippy objects, split `owned` into `refusal(out) -> Response` for the non-`Done` arms.)

- [ ] **Step 3: Run and commit**

Gate. Commit: `feat(web): share links, leaving and removing people`.

---

### Task 10: The join page

**Files:**
- Create: `crates/scout-web/src/routes/join.rs`
- Modify: `crates/scout-web/src/routes/mod.rs` (`pub mod join;`), `crates/scout-web/src/lib.rs` (~221, merge `routes::join::routes(auth.clone())`)
- Modify: `crates/scout-web/src/pages.rs` (join pages)
- Modify: `crates/scout-web/src/routes/auth.rs` (`signed_in` ~371 and its two callers)

- [ ] **Step 1: Write the failing tests** (in `join.rs`)

```rust
#[cfg(test)]
mod tests {
    use crate::tests::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn shared() -> (axum::Router, std::sync::Arc<scout_core::core::Core>, tempfile::TempDir, String) {
        let (app, core, dir) = test_app().await;
        open_round(&core, "autumn", 5).await;
        let scout_core::identity::SignIn::In { account_id } =
            scout_core::identity::sign_in(&core, "telegram", "777").await.unwrap()
        else { panic!() };
        scout_core::trips::seed_trip_for_tests(&core, account_id, "October").await.unwrap();
        let link = scout_core::sharing::link(&core, account_id, "October", "j".repeat(22)).await.unwrap().unwrap();
        (app, core, dir, link.token)
    }

    #[tokio::test]
    async fn a_stranger_sees_the_trip_and_both_ways_in_and_is_remembered_for_after() {
        let (app, _core, _dir, token) = shared().await;
        let res = app.clone().oneshot(Request::get(format!("/join/{token}")).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let cookie = res.headers()["set-cookie"].to_str().unwrap().to_string();
        assert!(cookie.starts_with(&format!("__Host-scout_join={token};")), "{cookie}");
        let page = body_of(res).await;
        assert!(page.contains("October"));
        assert!(page.contains("action=\"/sign-in/email\""));
    }

    #[tokio::test]
    async fn signing_in_with_the_join_cookie_returns_to_the_join_page() {
        let headers = {
            let mut h = axum::http::HeaderMap::new();
            h.insert("cookie", format!("__Host-scout_join={}", "j".repeat(22)).parse().unwrap());
            h
        };
        assert_eq!(super::after_sign_in(&headers), format!("/join/{}", "j".repeat(22)));
        let mut forged = axum::http::HeaderMap::new();
        forged.insert("cookie", "__Host-scout_join=..%2F..%2Fevil".parse().unwrap());
        assert_eq!(super::after_sign_in(&forged), "/chat");
    }

    #[tokio::test]
    async fn a_signed_in_newcomer_joins_takes_a_seat_and_lands_on_the_trip() {
        let (app, core, _dir, token) = shared().await;
        let stranger = core.store().account_for_telegram(5150).unwrap();
        let cookie = crate::session::mint(TEST_KEY, stranger, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, stranger);
        let page = app.clone().oneshot(
            Request::get(format!("/join/{token}"))
                .header("cookie", format!("{}={cookie}", crate::session::COOKIE))
                .body(Body::empty()).unwrap(),
        ).await.unwrap();
        assert!(body_of(page).await.contains("Join trip"));
        let res = app.clone().oneshot(
            Request::post(format!("/join/{token}"))
                .header("cookie", format!("{}={cookie}", crate::session::COOKIE))
                .header("origin", "https://example.com")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!("csrf={csrf}&name=Masha"))).unwrap(),
        ).await.unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers()["location"], "/chat?trip=October");
        assert!(core.store().is_member(stranger).unwrap());
    }

    #[tokio::test]
    async fn a_dead_link_says_so_and_offers_no_sign_in() {
        let (app, _core, _dir, _token) = shared().await;
        let res = app.oneshot(Request::get(format!("/join/{}", "z".repeat(22))).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let page = body_of(res).await;
        assert!(page.contains("This link does not work"));
        assert!(!page.contains("/sign-in/email"));
    }
}
```

- [ ] **Step 2: The pages** (in `pages.rs`)

```rust
/// What a share link opens, before the visitor is known: the trip's name,
/// its dates and its people, then the two ways in. `trip` and the names
/// came from people, so they are escaped.
pub fn join_signed_out(trip: &str, dates: &str, people: &[String], csrf: &str, widget: Option<&str>) -> String {
    let telegram = widget
        .map(|w| format!(r#"<p class="muted">Or with Telegram:</p><div class="card">{w}</div>"#))
        .unwrap_or_default();
    shell(
        "Join a trip",
        &format!(
            r#"  <h1>Join {trip}</h1>
  <p>{dates}{sep}With {people}.</p>
  <p>Sign in to see the plan and add to it. If you are new to Scout, this signs you up.</p>
  <div class="card">
    <form method="post" action="/sign-in/email">
      <input type="hidden" name="csrf" value="{csrf}">
      <label for="email">Email address</label>
      <input type="email" id="email" name="email" autocomplete="email" required placeholder="you@example.com">
      <button class="btn" type="submit">Email me a link</button>
    </form>
  </div>{telegram}"#,
            trip = escape(trip),
            sep = if dates.is_empty() { "" } else { " " },
            dates = escape(dates),
            people = escape(&people.join(", ")),
        ),
    )
}

/// The signed-in visitor's form: what to be called on the trip, and, when
/// they already have a trip of this name, a new name for theirs.
pub fn join_form(trip: &str, token: &str, csrf: &str, name: &str, clash: Option<&str>, problem: Option<&str>, in_telegram: bool) -> String {
    let rename = clash
        .map(|c| format!(
            r#"<p>You already have a trip called {c}. Give yours another name to join this one.</p>
      <label for="rename">New name for your trip</label>
      <input type="text" id="rename" name="rename" required maxlength="80">"#,
            c = escape(c)
        ))
        .unwrap_or_default();
    let problem = problem.map(|p| format!(r#"<p class="muted">{}</p>"#, escape(p))).unwrap_or_default();
    let surface = if in_telegram { r#"<input type="hidden" name="in" value="telegram">"# } else { "" };
    shell(
        "Join a trip",
        &format!(
            r#"  <h1>Join {trip}</h1>
  <div class="card">
    <form method="post" action="/join/{token}">
      <input type="hidden" name="csrf" value="{csrf}">{surface}
      <label for="name">Your name on this trip</label>
      <input type="text" id="name" name="name" value="{name}" required maxlength="40">
      {rename}{problem}
      <button class="btn" type="submit">Join trip</button>
    </form>
  </div>"#,
            trip = escape(trip),
            name = escape(name),
        ),
    )
}

/// A link that cannot be used, and who to ask.
pub fn join_dead(headline: &str, sentence: &str) -> String {
    shell("Join a trip", &format!("  <h1>{}</h1>\n  <p>{}</p>", escape(headline), escape(sentence)))
}
```

Add `input[type=text]` to the shell's `input[type=email]` rule: `input[type=email],input[type=text]{…}`.

- [ ] **Step 3: The routes** (`routes/join.rs`)

```rust
//! `/join/{token}`: what a share link opens. Public — the visitor may not
//! have an account yet — and the only page that admits someone to Scout
//! without an invite round, which is the point of a share link.

use super::{see_other, signed_in_as, sorry};
use crate::routes::trips::is_link_token;
use crate::{pages, session, AuthState};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Form, Router};
use scout_core::sharing::{Joined, LinkState};

pub(crate) const JOIN_COOKIE: &str = "__Host-scout_join";

pub fn routes(auth: AuthState) -> Router {
    Router::new().route("/join/{token}", get(page).post(join)).with_state(auth)
}

/// Where signing in should land: back on the join page the visitor came
/// from, if the cookie it set holds a token's shape, else the chat.
pub(crate) fn after_sign_in(headers: &HeaderMap) -> String {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|jar| session::read_cookie(jar, JOIN_COOKIE))
        .filter(|t| is_link_token(t))
        .map(|t| format!("/join/{t}"))
        .unwrap_or_else(|| "/chat".to_string())
}

#[derive(serde::Deserialize, Default)]
struct Surface {
    #[serde(rename = "in")]
    surface: Option<String>,
}

fn dates(preview: &scout_core::sharing::Preview) -> String {
    match (&preview.first, &preview.last) {
        (Some(first), Some(last)) if first != last => format!("{first} to {last}."),
        (Some(first), _) => format!("{first}."),
        _ => String::new(),
    }
}

fn dead(state: &LinkState, owner: &str) -> Response {
    let (headline, sentence) = match state {
        LinkState::Expired => ("This link has expired", format!("Links last 14 days. Ask {owner} for a new one.")),
        LinkState::Off => ("This link was turned off", format!("Ask {owner} for a new one.")),
        _ => ("This link does not work", "Check that you copied all of it, or ask for a new one.".to_string()),
    };
    (StatusCode::NOT_FOUND, Html(pages::join_dead(headline, &sentence))).into_response()
}

fn framed(response: Response, in_telegram: bool) -> Response {
    if in_telegram { super::telegram_app::framable(response) } else { response }
}

async fn page(State(auth): State<AuthState>, headers: HeaderMap, Path(token): Path<String>, Query(q): Query<Surface>) -> Response {
    let in_telegram = q.surface.as_deref() == Some("telegram");
    if !is_link_token(&token) {
        return dead(&LinkState::Unknown, "whoever sent it");
    }
    let preview = match scout_core::sharing::preview(&auth.core, &token).await {
        Ok(Some(preview)) => preview,
        Ok(None) => return dead(&LinkState::Unknown, "whoever sent it"),
        Err(e) => {
            tracing::error!(error = %e, "could not read a share link");
            return sorry();
        }
    };
    let owner = preview.people.first().cloned().unwrap_or_else(|| "the owner".into());
    if !matches!(preview.state, LinkState::Live(_)) {
        return framed(dead(&preview.state, &owner), in_telegram);
    }
    let Some(account_id) = signed_in_as(&auth, &headers) else {
        let widget = super::auth::widget(&auth, None);
        let html = pages::join_signed_out(&preview.trip, &dates(&preview), &preview.people, &session::csrf(&auth.cfg.session_key), widget.as_deref());
        let cookie = format!("{JOIN_COOKIE}={token}; Path=/; Max-Age=3600; Secure; HttpOnly; SameSite=Lax");
        return ([(header::SET_COOKIE, cookie)], Html(html)).into_response();
    };
    // Already on it — this trip, not one of theirs that shares its name:
    // straight to the trip.
    if let Ok(Some(plan)) = scout_core::trips::find(&auth.core, account_id, &preview.trip).await {
        if plan.people.len() > 1 && plan.people.iter().any(|p| p.you) && plan.people.iter().map(|p| &p.name).eq(preview.people.iter()) {
            return see_other(&trip_path(&preview.trip, in_telegram));
        }
    }
    let name = scout_core::sharing::suggested_name(&auth.core, account_id).await.unwrap_or_else(|_| "Traveller".into());
    let csrf = session::csrf_for(&auth.cfg.session_key, account_id);
    framed(Html(pages::join_form(&preview.trip, &token, &csrf, &name, None, None, in_telegram)).into_response(), in_telegram)
}

fn trip_path(trip: &str, in_telegram: bool) -> String {
    let trip: String = form_urlencoded::byte_serialize(trip.as_bytes()).collect();
    if in_telegram { format!("/chat?in=telegram&trip={trip}") } else { format!("/chat?trip={trip}") }
}

#[derive(serde::Deserialize)]
struct JoinIn {
    csrf: String,
    name: String,
    rename: Option<String>,
    #[serde(rename = "in")]
    surface: Option<String>,
}

async fn join(State(auth): State<AuthState>, headers: HeaderMap, Path(token): Path<String>, Form(form): Form<JoinIn>) -> Response {
    let in_telegram = form.surface.as_deref() == Some("telegram");
    let Some(account_id) = signed_in_as(&auth, &headers) else {
        return see_other(&format!("/join/{token}"));
    };
    if !is_link_token(&token) || !session::csrf_ok_for(&auth.cfg.session_key, &form.csrf, account_id) {
        return (StatusCode::BAD_REQUEST, Html(pages::stale_form())).into_response();
    }
    let joined = match scout_core::sharing::join(&auth.core, account_id, &token, &form.name, form.rename.as_deref()).await {
        Ok(joined) => joined,
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not join a trip");
            return sorry();
        }
    };
    let csrf = session::csrf_for(&auth.cfg.session_key, account_id);
    let again = |trip: &str, clash: Option<&str>, problem: Option<&str>| {
        framed(Html(pages::join_form(trip, &token, &csrf, &form.name, clash, problem, in_telegram)).into_response(), in_telegram)
    };
    match joined {
        Joined::Joined { trip, seated } => {
            tracing::info!(account_id, seated, "joined a shared trip");
            let clear = format!("{JOIN_COOKIE}=; Path=/; Max-Age=0; Secure; HttpOnly; SameSite=Lax");
            let mut response = see_other(&trip_path(&trip, in_telegram));
            response.headers_mut().insert(header::SET_COOKIE, clear.parse().expect("a static cookie"));
            response
        }
        Joined::Already { trip } => see_other(&trip_path(&trip, in_telegram)),
        Joined::Link(state) => framed(dead(&state, "the person who sent it"), in_telegram),
        Joined::Revoked => framed(
            (StatusCode::FORBIDDEN, Html(pages::join_dead("You no longer have access to Scout", "Your seat was removed, so this link cannot add you."))).into_response(),
            in_telegram,
        ),
        Joined::Full => framed(
            (StatusCode::FORBIDDEN, Html(pages::join_dead("This link is full", "It has let in the 10 new people it can. Ask the person who sent it for a new one."))).into_response(),
            in_telegram,
        ),
        Joined::Clash { name, yours: true } => again(&name, Some(&name), None),
        Joined::Clash { name, yours: false } => framed(
            Html(pages::join_dead("You are already on a trip with this name", &format!("Leave {name} first, then open this link again."))).into_response(),
            in_telegram,
        ),
        Joined::RenameTaken { name } => again(&name, Some(&name), Some("You already have a trip with that name too. Pick another.")),
    }
}
```

The `Clash` arm's first argument is the joining trip's name in the form header; use `preview`-free `trip` from a `Joined::Clash` that carries both if the heading reads wrong in the browser check.

Make `telegram_app::framable` and `auth::widget` reachable (`pub(crate)` — `widget` already is).

- [ ] **Step 4: Sign-in returns to the join page**

In `auth.rs`, `signed_in` gains the request headers:

```rust
fn signed_in(auth: &AuthState, headers: &HeaderMap, account_id: i64) -> Response {
    // The chat is what people came for, unless they were on their way into
    // a shared trip: `after_sign_in` reads the join page's cookie.
    signed_in_at(auth, account_id, &super::join::after_sign_in(headers))
}
```

`telegram_callback` passes its `headers`; `confirm` gains a `headers: HeaderMap` extractor before `Form` and passes it. Run `cargo test -p scout-web auth` to see the existing sign-in tests still land on `/chat`.

- [ ] **Step 5: Mount it and run**

In `lib.rs`, `.merge(routes::join::routes(auth.clone()))` beside the trips routes. Gate. Commit: `feat(web): the join page, and signing in returns to it`.

---

### Task 11: Joining from the Mini App

**Files:** Modify `crates/scout-web/src/telegram_login.rs`, `crates/scout-web/src/routes/telegram_app.rs` (`start_session` ~96), `crates/scout-web/src/tg.js`

- [ ] **Step 1: Write the failing tests** (in `telegram_app.rs` tests, beside the existing session tests; reuse their signing helper `web_app_data_like_telegram`)

```rust
    #[tokio::test]
    async fn a_launch_from_a_share_link_signs_in_a_stranger_and_sends_them_to_join() {
        let (app, core, _dir) = test_app().await;
        let owner = core.store().account_for_telegram(1).unwrap();
        scout_core::trips::seed_trip_for_tests(&core, owner, "October").await.unwrap();
        let link = scout_core::sharing::link(&core, owner, "October", "k".repeat(22)).await.unwrap().unwrap();
        let user = r#"{"id":4242,"first_name":"Masha"}"#;
        let start = format!("j_{}", link.token);
        let auth_date = chrono::Utc::now().timestamp().to_string();
        let data = crate::telegram_login::web_app_data_like_telegram(
            TEST_BOT_TOKEN,
            &[("auth_date", &auth_date), ("user", user), ("start_param", &start)],
        );
        let res = post_session(&app, &data).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_eq!(body["next"], format!("/join/{}?in=telegram", link.token));
    }
```

`TEST_BOT_TOKEN` and `post_session` stand for whatever the existing tests in this module call the bot token and the POST helper — use those names.

- [ ] **Step 2: Implement**

In `telegram_login.rs`:

```rust
/// The `start_param` of launch data already verified by `verify_web_app`:
/// what a `?startapp=` link carried. Read separately so `verify_web_app`
/// keeps answering one question.
pub fn start_param(init_data: &str) -> Option<String> {
    form_urlencoded::parse(init_data.as_bytes())
        .find(|(k, _)| k == "start_param")
        .map(|(_, v)| v.into_owned())
}
```

In `start_session`, after `account_id` is known and before `is_admitted`:

```rust
    // A share link's launch: the join page decides who may join, seat and
    // all, so the cookie is handed out whatever the account's standing.
    let joining = telegram_login::start_param(&body.init_data)
        .and_then(|p| p.strip_prefix("j_").map(str::to_string))
        .filter(|t| crate::routes::trips::is_link_token(t));
    if let Some(token) = joining {
        let cookie = session::mint(&auth.cfg.session_key, account_id, session::SESSION_TTL_SECS);
        return (
            StatusCode::OK,
            [(header::SET_COOKIE, session::set_embedded_cookie(&cookie, session::SESSION_TTL_SECS))],
            Json(serde_json::json!({ "next": format!("/join/{token}?in=telegram") })),
        )
            .into_response();
    }
```

In `tg.js`, replace the `if (res.ok) { … }` block:

```js
  if (res.ok) {
    markExchanged(store, Date.now())
    // A share link's launch answers with where to go: its join page.
    const next = res.status === 200 ? (await res.json().catch(() => ({}))).next : null
    location.replace(typeof next === 'string' && next.startsWith('/join/') ? next : tripPageUrl(location.search))
    return
  }
```

- [ ] **Step 3: Run and commit**

Gate. Commit: `feat(telegram): a share link opens the join page inside the Mini App`.

---

### Task 12: The page's words, tested

**Files:** Modify `crates/scout-web/src/chat.js` (pure functions near `tripDeleteConsequence` ~1041), `crates/scout-web/src/chat.test.mjs`

- [ ] **Step 1: Write the failing tests** (append to `chat.test.mjs`, and add the four names to its import)

```js
test('the people line names the reader first and says when a trip is theirs alone', () => {
  assert.equal(peopleLine({ people: [{ name: 'Sasha', owner: true, you: true }] }), 'Only you')
  assert.equal(peopleLine({ people: [] }), 'Only you')
  const shared = { people: [{ name: 'Sasha', owner: true, you: false }, { name: 'Masha', owner: false, you: true }, { name: 'Dan', owner: false, you: false }] }
  assert.equal(peopleLine(shared), 'You, Sasha, Dan')
})

test('an item says who added it only on a shared trip', () => {
  const solo = { people: [{ name: 'Sasha', owner: true, you: true }] }
  const shared = { people: [{ name: 'Sasha', owner: true, you: true }, { name: 'Masha', owner: false, you: false }] }
  const by = (via, name, you = false) => ({ added: { via, name, you } })
  assert.equal(addedLabel(by('page', 'Masha'), solo), null)
  assert.equal(addedLabel({ added: null }, shared), null)
  assert.equal(addedLabel(by('page', 'Masha'), shared), 'Added by Masha')
  assert.equal(addedLabel(by('scout', 'Masha'), shared), 'Added by Masha, via Scout')
  assert.equal(addedLabel(by('mail', 'Sasha', true), shared), 'Added by you, from mail')
  assert.equal(addedLabel(by('page', null), shared), 'Added by someone who left')
})

test('the link line says until when and how many of its places are used', () => {
  assert.equal(linkStateLine({ expires: '2026-10-25', admits: 2, cap: 10 }, 'en-GB'), 'Link works until 25 Oct · 2 of 10 new people used')
  assert.equal(linkStateLine(null), 'No link yet. Copy link makes one.')
})

test('a person is removed by id, a trip shared and left by name', () => {
  assert.deepEqual(JSON.parse(removePersonBody('Lisbon', 7)), { trip: 'Lisbon', person: 7 })
  assert.deepEqual(JSON.parse(tripBody('Lisbon')), { trip: 'Lisbon' })
})
```

- [ ] **Step 2: Implement** (exported, beside `tripDeleteConsequence`)

```js
// "You, Sasha, Dan": the reader first, as "You", then everyone else in the
// order the trip lists them — the owner first. "Only you" for a trip of
// one, which is every trip until it is shared.
export function peopleLine(trip) {
  const people = trip?.people ?? []
  if (people.length < 2) return 'Only you'
  const others = people.filter((p) => !p.you).map((p) => p.name)
  return ['You', ...others].join(', ')
}

const ADDED_VIA = { scout: ', via Scout', mail: ', from mail' }

// Shown only while more than one person is on the trip: on a trip of one
// every card would say "Added by you", which says nothing.
export function addedLabel(item, trip) {
  if (!item?.added || (trip?.people?.length ?? 0) < 2) return null
  const who = item.added.you ? 'you' : (item.added.name ?? 'someone who left')
  return `Added by ${who}${ADDED_VIA[item.added.via] ?? ''}`
}

export function linkStateLine(link, locale = undefined) {
  if (!link) return 'No link yet. Copy link makes one.'
  const until = new Date(`${link.expires}T12:00:00Z`).toLocaleDateString(locale, { day: 'numeric', month: 'short', timeZone: 'UTC' })
  return `Link works until ${until} · ${link.admits} of ${link.cap} new people used`
}

export function tripBody(tripName) {
  return JSON.stringify({ trip: tripName })
}

export function removePersonBody(tripName, person) {
  return JSON.stringify({ trip: tripName, person })
}
```

(`author_names` keeps the names of people who left, so `name` is null only for an author whose row never existed — the fallback wording covers it.)

- [ ] **Step 3: Run and commit**

Run: `node --test 'crates/scout-web/src/*.test.mjs'` → PASS. Commit: `feat(web): the words for people, authors and the link`.

---

### Task 13: The People line, its panel and the labels

**Files:** Modify `crates/scout-web/src/chat.js` (`renderTripDetail` ~2271, `renderOtherItem` ~2196, `renderItem` ~2232, `start` ~4400), `crates/scout-web/src/chat.html` (CSS)

- [ ] **Step 1: The label on every card**

At the end of `renderOtherItem` and of `renderItem` (both return paths: before `return card` after the "no options" line, and at the end), add:

```js
    const added = addedLabel(segment, trip)
    if (added) card.append(node('p', 'item-added', added))
```

(`item` instead of `segment` in `renderOtherItem`.)

- [ ] **Step 2: The People line in the header**

In `renderTripDetail`, after the `trip-subtitle` line is appended to `title`:

```js
    const peopleButton = node('button', 'trip-people', `${peopleLine(trip)} · Share`)
    peopleButton.type = 'button'
    peopleButton.setAttribute('aria-expanded', 'false')
    title.append(peopleButton)
```

After `tripDetail.append(deleteSlot)`:

```js
    const peopleSlot = node('div', 'people-slot')
    tripDetail.append(peopleSlot)
    peopleButton.addEventListener('click', () => {
      const open = peopleButton.getAttribute('aria-expanded') === 'true'
      peopleButton.setAttribute('aria-expanded', String(!open))
      peopleSlot.replaceChildren(...(open ? [] : [peoplePanel(trip)]))
    })
```

The delete × is the owner's alone: wrap its creation, `actions.append(deleteButton)` and its listener in `if (trip.you_own !== false) { … }`.

- [ ] **Step 3: The panel**

Beside `tripDeleteConfirm`:

```js
  // Who is on the trip and what can be done about it: copy the link for
  // anyone; leave for an editor; remove people and manage the link for
  // the owner. Each action answers with the trips, so the page repaints
  // from the answer as every other trip write does.
  function peoplePanel(trip) {
    const panel = node('section', 'people-panel')
    panel.setAttribute('aria-label', `People on ${trip.name}`)
    const list = node('ul', 'people-list')
    for (const person of trip.people ?? []) {
      const row = node('li', 'people-row')
      row.append(node('span', 'people-name', person.you ? `${person.name} (you)` : person.name))
      if (person.owner) row.append(node('span', 'people-owner', 'owner'))
      if (trip.you_own && !person.owner) {
        const remove = node('button', 'people-remove', 'Remove')
        remove.type = 'button'
        remove.addEventListener('click', () => confirmThen(remove, `Remove ${person.name}`, () => tripAction('/chat/trips/remove-person', removePersonBody(trip.name, person.id))))
        row.append(remove)
      }
      list.append(row)
    }
    panel.append(list)
    const actions = node('div', 'people-actions')
    const copy = node('button', 'people-copy', 'Copy link')
    copy.type = 'button'
    copy.addEventListener('click', () => copyShareLink(trip, copy))
    actions.append(copy)
    if (trip.you_own) {
      for (const [label, path] of [['New link', '/chat/trips/link-new'], ['Turn link off', '/chat/trips/link-off']]) {
        const button = node('button', 'people-link-action', label)
        button.type = 'button'
        button.addEventListener('click', () => tripAction(path, tripBody(trip.name), { reload: true }))
        actions.append(button)
      }
      panel.append(actions, node('p', 'people-link-state', linkStateLine(trip.link)))
    } else {
      const leave = node('button', 'people-leave', 'Leave trip')
      leave.type = 'button'
      leave.addEventListener('click', () => confirmThen(leave, 'Leave trip', () => tripAction('/chat/trips/leave', tripBody(trip.name))))
      actions.append(leave)
      panel.append(actions)
    }
    return panel
  }

  // Asks once, in place: the button reads its question, and a second
  // press within four seconds does it — the arming Remove already uses.
  function confirmThen(button, label, act) {
    if (button.dataset.armed === 'true') {
      button.disabled = true
      act().finally(() => { button.disabled = false })
      return
    }
    button.dataset.armed = 'true'
    button.textContent = `${label}?`
    setTimeout(() => { button.dataset.armed = 'false'; button.textContent = label.split(' ')[0] === 'Remove' ? 'Remove' : label }, 4000)
  }

  async function tripAction(path, body, { reload = false } = {}) {
    const res = await fetch(path, { method: 'POST', headers: jsonHeaders(), body })
    if (!res.ok) {
      showTripToast(res.status === 403 ? 'Only the trip’s owner can do that.' : 'That did not work. Reload and try again.')
      return
    }
    if (reload) {
      tripsLoaded = false
      await loadTrips()
      showTripToast('Done.')
      return
    }
    trips = await res.json()
    if (!trips.some((t) => t.name === currentTrip)) currentTrip = trips[0]?.name ?? null
    renderTripList()
    renderTripDetail()
  }

  async function copyShareLink(trip, button) {
    const res = await fetch('/chat/trips/link', { method: 'POST', headers: jsonHeaders(), body: tripBody(trip.name) })
    if (!res.ok) { showTripToast('Could not make a link. Try again.'); return }
    const link = await res.json()
    const url = inTelegram && link.telegram_url ? link.telegram_url : link.url
    try {
      await navigator.clipboard.writeText(url)
      showTripToast('Link copied. It works for 14 days.')
    } catch {
      showTripToast(url)
    }
    button.blur()
  }
```

`jsonHeaders()` stands for however the existing trip writes build `content-type` plus `x-scout-csrf` (see `deleteTrip` / `keepTrip`); use that helper by its real name. `inTelegram` is the existing flag set at start.

- [ ] **Step 4: `?trip=` on the website**

In `start()`, after `loadTrips().catch(() => {})` at the end of the non-Telegram path:

```js
  // A join lands here as /chat?trip=<name>: open that trip.
  const asked = new URLSearchParams(location.search).get('trip')
  if (asked) {
    currentTrip = asked
    switchView('trips')
  }
```

- [ ] **Step 5: CSS** (in `chat.html`, beside `.trip-subtitle`; tokens only — the theme tests reject literal colours)

```css
.trip-people{margin-top:6px; padding:4px 0; border:0; background:none; color:var(--blue); font:inherit; font-size:14px; cursor:pointer; text-align:left}
.trip-people:hover{text-decoration:underline}
.trip-people:focus-visible{outline:2px solid var(--blue); outline-offset:2px}
.people-panel{margin:0 0 14px; padding:14px 16px; border:1px solid var(--line); border-radius:6px; background:var(--card)}
.people-list{list-style:none; margin:0 0 12px; padding:0; display:grid; gap:8px}
.people-row{display:flex; align-items:center; gap:10px}
.people-name{color:var(--base1)}
.people-owner{font-size:12px; letter-spacing:.06em; text-transform:uppercase; color:var(--base0)}
.people-remove{margin-left:auto}
.people-actions{display:flex; flex-wrap:wrap; gap:8px}
.people-actions button,.people-remove{padding:6px 12px; border:1px solid var(--line); border-radius:5px; background:transparent; color:var(--base1); font:inherit; font-size:14px; cursor:pointer}
.people-actions button:hover,.people-remove:hover{background:var(--row-hover)}
.people-link-state{margin:10px 0 0; font-size:13px; color:var(--base0)}
.item-added{margin:8px 0 0; font-size:13px; color:var(--base0)}
```

Use whichever existing token names match `--line`, `--card`, `--row-hover` in `chat.html`'s `:root`; `cargo test -p scout-web chat_page` runs the theme tests that check every colour comes from a token every theme defines.

- [ ] **Step 6: Run and commit**

Gate. Commit: `feat(web): the People line, its panel, and who added each card`.

---

### Task 14: The label on paper

**Files:** Modify `crates/scout-web/src/trip_pdf.rs` (the item loop ~1070 and the flight branch after it)

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn a_shared_trips_items_say_who_added_them_on_paper() {
        let mut plan = sample_plan();
        plan.people = vec![
            scout_core::trips::Person { id: 1, name: "Sasha".into(), owner: true, you: true },
            scout_core::trips::Person { id: 2, name: "Masha".into(), owner: false, you: false },
        ];
        plan.trip.items[0].added = Some(scout_core::trips::Added { account_id: 2, via: "scout".into(), name: Some("Masha".into()), you: false });
        assert!(html_of(&plan).contains("Added by Masha, via Scout"));
        plan.people.truncate(1);
        assert!(!html_of(&plan).contains("Added by"));
    }
```

`sample_plan` and `html_of` stand for the module's existing fixture and HTML builder used by its other tests; re-export `Added` from `scout_core::trips` (`pub use crate::store::{Added, …}`) if it is not already reachable.

- [ ] **Step 2: Implement**

```rust
/// "Added by Masha, via Scout" on a shared trip; nothing on a trip of one.
/// The reader's own items say their name, not "you": paper is handed on.
fn added_line(plan: &Plan, item: &TripItem) -> Option<String> {
    if plan.people.len() < 2 {
        return None;
    }
    let added = item.added.as_ref()?;
    let name = if added.you {
        plan.people.iter().find(|p| p.you).map(|p| p.name.clone())
    } else {
        added.name.clone()
    }?;
    let via = match added.via.as_str() {
        "scout" => ", via Scout",
        "mail" => ", from mail",
        _ => "",
    };
    Some(format!("Added by {name}{via}"))
}
```

Add `added_line(plan, segment).map(|l| format!("<span>{}</span>", escape(&l)))` to the `meta_line(&[…])` list for non-flight items, and to the flight card's meta list in the same way.

- [ ] **Step 3: Run and commit**

Gate. Commit: `feat(pdf): a shared trip's items say who added them`.

---

### Task 15: Verify, ship, record

- [ ] **Step 1: The full gate**

```bash
cargo test --workspace 2>&1 | grep -E "^test result|FAILED|panicked"
cargo clippy --workspace --all-targets -- -D warnings
node --test 'crates/scout-web/src/*.test.mjs'
```

Expected: every `test result: ok`, clippy clean, node all pass.

- [ ] **Step 2: Browser check on the stand-in server**

Extend the scratch stand-in (`scratchpad/apptheme/server.mjs`) with two accounts' trip lists: `GET /chat/trips` answering a shared trip whose `people` has two entries, `you_own` toggled by a query flag, one item with `added: {via:'scout', name:'Masha', you:false}`; `POST /chat/trips/link` answering `{url, telegram_url, expires, admits, cap}`. At 390px and 1280px, check: the People line reads "You, Masha · Share"; the panel opens under the header; the owner sees Remove, New link, Turn link off and the link line; the editor sees Leave trip and no delete ×; the label sits at the foot of the card; a solo trip shows "Only you · Share" and no labels. Screenshot each state once.

- [ ] **Step 3: Merge and push**

```bash
git checkout main && git merge --no-ff feat/shared-trips-people -m "Merge: people on a trip — share links, joining, labels

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" && git push
```

- [ ] **Step 4: Telegram's Main Mini App** — the user sets it once: BotFather → the bot → Bot Settings → Configure Mini App → Main App URL `https://goodscout.fyi/tg`. Until then `telegram_url` opens the bot chat without the join page; the web URL works everywhere.

- [ ] **Step 5: Deploy and watch the migration**

```bash
scripts/deploy-k3s.sh
ssh $(sed -n 's/^SCOUT_SSH=//p' .env) 'kubectl -n scout logs -l app=scout --tail=80' | grep -E "schema|migrat|scout is up|ERROR"
```

Expected: a backup taken for version 21, the migration applied, "scout is up". No crash loop.

- [ ] **Step 6: Live check** — open the live trips page signed in: the People line shows "Only you · Share" on an existing trip, existing cards show no label, Copy link answers with a `/join/…` URL. Do not join with a second account on live data without the user's say-so.

- [ ] **Step 7: Record** — `docs/BOARD.md` Done: "People on a trip: share links, joining, labels (`<merge sha>`)"; move the card on the board artifact (`https://claude.ai/code/artifact/9be3e94c-0416-4f18-86a6-528a92847c64`, collection `cards`) to Done. Commit and push the board line.
