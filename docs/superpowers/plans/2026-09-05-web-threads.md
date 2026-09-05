# Threads in the Browser — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The web chat lists a member's threads, lets them switch, rename, pin and delete, and unpinned threads vanish after 48 idle hours.

**Architecture:** Two columns on `conversations` (`title`, `pinned`). "Current" stays what `session::latest_direct` returns — newest `direct` conversation by `updated_at` — so Telegram and the mirror follow a switch for free. Store gains account-keyed thread methods and an expiry; `session.rs` wraps them; `routes/chat.rs` exposes them; `chat.html`/`chat.js` grow a sidebar. Spec: `docs/superpowers/specs/2026-09-05-web-threads-design.md`.

**Tech Stack:** Rust workspace (scout-core: DuckDB via `duckdb` crate, rig; scout-web: axum 0.8, tower for tests), vanilla ES module client tested with `node --test`.

**Conventions you must follow:**
- Every store method takes `&self`, gets the connection with `self.conn()`, and returns `anyhow::Result`.
- Async code never calls the store directly; it goes through `crate::core::blocking(move || …)`.
- Test names are sentences in snake_case that say what would be wrong if they failed.
- Run tests with `cargo test -p <crate> --lib -- <filter>`. Run the JS tests with `node --test 'crates/scout-web/src/*.test.mjs'` from the repo root.
- Commit after every task with a message shaped like the repo's: `feat: …`, `fix: …`, `test: …`, lowercase, describing the behaviour, ending with `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`.
- Do not run `cargo fmt` on files; the repo is not fmt-clean.

---

### Task 1: Two columns on `conversations`

**Status: done in ea84cd1 and the fix commit after it; the text below is what shipped.**

**Files:**
- Modify: `crates/scout-core/src/store.rs` — `MIGRATIONS` (the `conversations` table near line 219), the `steps()` list near line 698, a new `STEP_7_THREADS` const beside `STEP_6_LOGIN_TOKENS`.
- Test: `crates/scout-core/src/store.rs` tests module (append before the final `}`).

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `store.rs`:

```rust
    fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM information_schema.columns
                 WHERE table_name = ? AND column_name = ?",
                params![table, column],
                |r| r.get(0),
            )
            .unwrap();
        n > 0
    }

    #[test]
    fn a_fresh_database_has_a_title_and_a_pin_on_every_conversation() {
        let (s, _dir) = test_store();
        let conn = s.conn();
        assert!(has_column(&conn, "conversations", "title"));
        assert!(has_column(&conn, "conversations", "pinned"));
    }

    #[test]
    fn a_database_from_before_threads_grows_the_two_columns_and_keeps_its_rows() {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("legacy.duckdb");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(LEGACY_SCHEMA).unwrap();
        }
        let s = Store::open(&db).unwrap();
        // Step 6 was the last one before this; a database that stops there
        // is exactly the production file.
        assert_eq!(s.schema_version().unwrap(), 8, "the threads steps did not run");
        let a = s.account_for_telegram(11).unwrap();
        let id = s.start_conversation(a, "direct").unwrap();
        let conn = s.conn();
        assert!(has_column(&conn, "conversations", "title"));
        let pinned: bool = conn
            .query_row("SELECT pinned FROM conversations WHERE id = ?", params![id], |r| r.get(0))
            .unwrap();
        assert!(!pinned, "a thread starts unpinned");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p scout-core --lib -- store::tests::a_fresh_database_has_a_title store::tests::a_database_from_before_threads`
Expected: both FAIL — the fresh one on `has_column(... "title")` being false, the legacy one on `schema_version() >= 7`.

- [ ] **Step 3: Add the columns to `MIGRATIONS` and a step 7**

In `MIGRATIONS`, change the `conversations` table to:

```sql
CREATE TABLE IF NOT EXISTS conversations (
    id            BIGINT PRIMARY KEY DEFAULT nextval('conversations_id_seq'),
    account_id    BIGINT NOT NULL,
    scope         TEXT NOT NULL,
    pending_draft TEXT,
    started_at    TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at    TIMESTAMP NOT NULL DEFAULT current_timestamp,
    -- Last two, so a fresh database and a migrated one — where these
    -- arrive by ALTER TABLE in step 7 — have the same column order.
    -- What the sidebar calls it. Null until the first answer lands; then
    -- the first message trimmed, unless someone renamed it. See the
    -- threads design doc.
    title         TEXT,
    -- "Permanent": exempt from the 48-hour expiry. Nothing else.
    pinned        BOOLEAN NOT NULL DEFAULT false
);
```

Below `STEP_6_LOGIN_TOKENS` add:

```rust
/// Threads in the browser: a name and a pin.
///
/// The `pinned` column is added bare, backfilled and given its default
/// here, and made NOT NULL in step 8 — a separate step because DuckDB
/// refuses `SET NOT NULL` in a transaction that has already touched the
/// table's rows, and the `ADD COLUMN` in this one counts. `apply_steps`
/// runs each step in its own transaction, so a separate step is what a
/// separate transaction costs. (`ADD COLUMN` with a constraint is refused
/// outright, which is why the constraint is not simply on the add.)
/// `IF NOT EXISTS` so a database created by `MIGRATIONS` after this
/// shipped, but somehow recorded below 7, is not broken by the step. The
/// backfill and the `SET DEFAULT` are separate statements, rather than
/// folding the default into `ADD COLUMN pinned BOOLEAN DEFAULT false`,
/// because `ADD COLUMN IF NOT EXISTS` may no-op on a file that already has
/// the column — from an interrupted prior run of this same step — and the
/// explicit backfill and default still need to run against that file too.
const STEP_7_THREADS: &str = r#"
ALTER TABLE conversations ADD COLUMN IF NOT EXISTS title TEXT;
ALTER TABLE conversations ADD COLUMN IF NOT EXISTS pinned BOOLEAN;
UPDATE conversations SET pinned = false WHERE pinned IS NULL;
ALTER TABLE conversations ALTER COLUMN pinned SET DEFAULT false;
"#;

/// See `STEP_7_THREADS`. Dying between 7 and 8 leaves a nullable column
/// that holds no nulls, and this runs alone on the next boot.
const STEP_8_PINNED_NOT_NULL: &str = r#"
ALTER TABLE conversations ALTER COLUMN pinned SET NOT NULL;
"#;
```

In `steps()` add `(7, Step::Sql(STEP_7_THREADS)),` and `(8, Step::Sql(STEP_8_PINNED_NOT_NULL)),` after step 6.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p scout-core --lib -- store::tests`
Expected: all store tests PASS, including the two new ones. If `a_pending_migration_backs_the_database_up_before_changing_it` asserts `>= 5` it still passes.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/store.rs
git commit -m "feat: a conversation has a title and a pin

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 2: Account-keyed thread methods on the store

**Status: done; see the commit log. The text below is what shipped.**

**Files:**
- Modify: `crates/scout-core/src/store.rs` — a `ThreadRow` struct near `PendingMirror` (line ~796), methods after `touch_conversation`.
- Test: same file, tests module.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn threads_are_listed_pinned_first_and_then_by_last_use() {
        let (s, _dir) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let old = s.start_conversation(a, "direct").unwrap();
        let pinned = s.start_conversation(a, "direct").unwrap();
        let newest = s.start_conversation(a, "direct").unwrap();
        s.conn()
            .execute_batch(&format!(
                "UPDATE conversations SET updated_at = CAST(current_timestamp AS TIMESTAMP) - to_seconds(3600) WHERE id = {old};
                 UPDATE conversations SET updated_at = CAST(current_timestamp AS TIMESTAMP) - to_seconds(7200) WHERE id = {pinned};"
            ))
            .unwrap();
        assert!(s.set_thread_pinned(a, pinned, true).unwrap());
        // Not this account's thread, and not `direct`: neither may appear.
        let b = s.account_for_telegram(22).unwrap();
        s.start_conversation(b, "direct").unwrap();
        s.start_conversation(a, "telegram:-100").unwrap();

        let ids: Vec<i64> = s.threads_of(a).unwrap().iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![pinned, newest, old]);
    }

    #[test]
    fn a_thread_row_carries_what_the_sidebar_shows() {
        let (s, _dir) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let id = s.start_conversation(a, "direct").unwrap();
        assert!(s.set_thread_title(a, id, "wasmiddel per kilo").unwrap());
        let rows = s.threads_of(a).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title.as_deref(), Some("wasmiddel per kilo"));
        assert!(!rows[0].pinned);
        assert!(rows[0].updated_at.ends_with('Z'), "not RFC 3339 UTC: {}", rows[0].updated_at);
    }

    #[test]
    fn opening_a_thread_makes_it_the_newest_and_only_for_its_owner() {
        let (s, _dir) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let first = s.start_conversation(a, "direct").unwrap();
        let second = s.start_conversation(a, "direct").unwrap();
        s.conn()
            .execute(
                "UPDATE conversations SET updated_at = CAST(current_timestamp AS TIMESTAMP) - to_seconds(60) WHERE id = ?",
                params![first],
            )
            .unwrap();
        assert_eq!(s.latest_conversation(a, "direct", 600).unwrap().map(|(id, _)| id), Some(second));

        assert!(s.open_conversation(a, first).unwrap());
        assert_eq!(s.latest_conversation(a, "direct", 600).unwrap().map(|(id, _)| id), Some(first));

        let stranger = s.account_for_telegram(22).unwrap();
        assert!(!s.open_conversation(stranger, first).unwrap(), "opened someone else's thread");
        assert!(!s.open_conversation(a, 999_999).unwrap(), "opened a thread that does not exist");
    }

    #[test]
    fn a_thread_can_only_be_renamed_pinned_or_deleted_by_its_owner() {
        let (s, _dir) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let stranger = s.account_for_telegram(22).unwrap();
        let id = s.start_conversation(a, "direct").unwrap();
        s.replace_messages(id, &["{}".to_string()]).unwrap();

        assert!(!s.set_thread_title(stranger, id, "mine now").unwrap());
        assert!(!s.set_thread_pinned(stranger, id, true).unwrap());
        assert!(!s.delete_conversation(stranger, id).unwrap());
        assert_eq!(s.threads_of(a).unwrap().len(), 1, "a stranger changed something");
        assert_eq!(s.thread_title(a, id).unwrap(), None);

        assert!(s.delete_conversation(a, id).unwrap());
        assert!(s.threads_of(a).unwrap().is_empty());
        assert!(s.conversation_messages(id, 10).unwrap().is_empty(), "messages outlived their thread");
    }

    #[test]
    fn a_title_written_only_when_missing_never_covers_a_rename() {
        let (s, _dir) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let id = s.start_conversation(a, "direct").unwrap();
        assert!(s.set_thread_title_if_missing(id, "first message").unwrap());
        assert!(!s.set_thread_title_if_missing(id, "second message").unwrap());
        assert_eq!(s.thread_title(a, id).unwrap().as_deref(), Some("first message"));
        s.set_thread_title(a, id, "renamed").unwrap();
        assert!(!s.set_thread_title_if_missing(id, "third").unwrap());
        assert_eq!(s.thread_title(a, id).unwrap().as_deref(), Some("renamed"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p scout-core --lib -- store::tests::threads_are_listed store::tests::a_thread_row store::tests::opening_a_thread store::tests::a_thread_can_only store::tests::a_title_written_only`
Expected: compile errors — `threads_of`, `set_thread_pinned`, `set_thread_title`, `open_conversation`, `delete_conversation`, `thread_title`, `set_thread_title_if_missing` not found.

- [ ] **Step 3: Implement**

Near `PendingMirror` add:

```rust
/// One row of the sidebar. `current` is not here: the store does not know
/// which thread a channel would continue, `session::latest_direct` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRow {
    pub id: i64,
    pub title: Option<String>,
    pub pinned: bool,
    /// RFC 3339, UTC.
    pub updated_at: String,
}
```

After `touch_conversation` add:

```rust
    /// The account's `direct` threads, pinned first, then newest use first.
    pub fn threads_of(&self, account_id: i64) -> Result<Vec<ThreadRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, title, pinned, strftime(updated_at, '%Y-%m-%dT%H:%M:%SZ')
             FROM conversations WHERE account_id = ? AND scope = 'direct'
             ORDER BY pinned DESC, updated_at DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![account_id], |r| {
            Ok(ThreadRow { id: r.get(0)?, title: r.get(1)?, pinned: r.get(2)?, updated_at: r.get(3)? })
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Bumps `updated_at`, which is what makes a thread current. `false`
    /// when the account does not own a `direct` thread by that id — which is
    /// also what a thread that no longer exists looks like, on purpose.
    pub fn open_conversation(&self, account_id: i64, conversation_id: i64) -> Result<bool> {
        let conn = self.conn();
        let n = conn.execute(
            "UPDATE conversations SET updated_at = now()
             WHERE id = ? AND account_id = ? AND scope = 'direct'",
            params![conversation_id, account_id],
        )?;
        Ok(n > 0)
    }

    pub fn thread_title(&self, account_id: i64, conversation_id: i64) -> Result<Option<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT title FROM conversations WHERE id = ? AND account_id = ?",
        )?;
        let row: Option<Option<String>> = stmt
            .query_map(params![conversation_id, account_id], |r| r.get(0))?
            .next()
            .transpose()?;
        Ok(row.flatten())
    }

    /// A rename. Owner-checked.
    pub fn set_thread_title(&self, account_id: i64, conversation_id: i64, title: &str) -> Result<bool> {
        let conn = self.conn();
        let n = conn.execute(
            "UPDATE conversations SET title = ? WHERE id = ? AND account_id = ?",
            params![title, conversation_id, account_id],
        )?;
        Ok(n > 0)
    }

    /// The automatic title after a first answer. Not owner-checked: the
    /// caller is `run_agent`, which already holds the conversation. Writes
    /// only over null, so a rename is never undone.
    pub fn set_thread_title_if_missing(&self, conversation_id: i64, title: &str) -> Result<bool> {
        let conn = self.conn();
        let n = conn.execute(
            "UPDATE conversations SET title = ? WHERE id = ? AND title IS NULL",
            params![title, conversation_id],
        )?;
        Ok(n > 0)
    }

    pub fn set_thread_pinned(&self, account_id: i64, conversation_id: i64, pinned: bool) -> Result<bool> {
        let conn = self.conn();
        let n = conn.execute(
            "UPDATE conversations SET pinned = ? WHERE id = ? AND account_id = ?",
            params![pinned, conversation_id, account_id],
        )?;
        Ok(n > 0)
    }

    /// The thread and its messages, in one transaction. Owner-checked.
    pub fn delete_conversation(&self, account_id: i64, conversation_id: i64) -> Result<bool> {
        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<bool> {
            let owned = conn.execute(
                "DELETE FROM conversations WHERE id = ? AND account_id = ?",
                params![conversation_id, account_id],
            )?;
            if owned == 0 {
                return Ok(false);
            }
            conn.execute("DELETE FROM messages WHERE conversation_id = ?", params![conversation_id])?;
            Ok(true)
        })();
        match result {
            Ok(deleted) => {
                conn.execute_batch("COMMIT")?;
                Ok(deleted)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p scout-core --lib -- store::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/store.rs
git commit -m "feat: a store that lists, opens, names, pins and deletes an account's threads

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 3: Expiry in the store

**Status: done; see the commit log. The text below is what shipped.**

**Files:**
- Modify: `crates/scout-core/src/store.rs` — after `delete_conversation`.
- Test: same tests module.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn expiry_takes_an_idle_unpinned_thread_and_leaves_a_pinned_or_recent_one() {
        let (s, _dir) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let idle = s.start_conversation(a, "direct").unwrap();
        let pinned = s.start_conversation(a, "direct").unwrap();
        let recent = s.start_conversation(a, "direct").unwrap();
        let group = s.start_conversation(a, "telegram:-100").unwrap();
        for id in [idle, pinned, recent, group] {
            s.replace_messages(id, &["{}".to_string()]).unwrap();
        }
        s.set_thread_pinned(a, pinned, true).unwrap();
        s.conn()
            .execute_batch(&format!(
                "UPDATE conversations SET updated_at = CAST(current_timestamp AS TIMESTAMP) - to_seconds(49 * 3600) WHERE id IN ({idle}, {pinned}, {group});
                 UPDATE conversations SET updated_at = CAST(current_timestamp AS TIMESTAMP) - to_seconds(47 * 3600) WHERE id = {recent};"
            ))
            .unwrap();

        let gone = s.expire_conversations(48 * 3600).unwrap();

        assert_eq!(gone, 2, "the idle direct thread and the idle group thread");
        let left: Vec<i64> = s.threads_of(a).unwrap().iter().map(|t| t.id).collect();
        assert_eq!(left, vec![pinned, recent]);
        assert!(s.conversation_messages(idle, 10).unwrap().is_empty(), "messages outlived their thread");
        assert_eq!(s.conversation_messages(pinned, 10).unwrap().len(), 1);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p scout-core --lib -- store::tests::expiry_takes`
Expected: compile error — `expire_conversations` not found.

- [ ] **Step 3: Implement**

```rust
    /// Deletes every unpinned conversation, any scope, idle for longer than
    /// `older_than_secs`, with its messages. Returns how many went.
    pub fn expire_conversations(&self, older_than_secs: i64) -> Result<usize> {
        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<usize> {
            conn.execute(
                "DELETE FROM messages WHERE conversation_id IN (
                     SELECT id FROM conversations
                     WHERE NOT pinned
                       AND updated_at < CAST(current_timestamp AS TIMESTAMP) - to_seconds(?))",
                params![older_than_secs],
            )?;
            let gone = conn.execute(
                "DELETE FROM conversations
                 WHERE NOT pinned
                   AND updated_at < CAST(current_timestamp AS TIMESTAMP) - to_seconds(?)",
                params![older_than_secs],
            )?;
            Ok(gone)
        })();
        match result {
            Ok(gone) => {
                conn.execute_batch("COMMIT")?;
                Ok(gone)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p scout-core --lib -- store::tests::expiry_takes`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/store.rs
git commit -m "feat: an idle, unpinned thread expires

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 4: `Thread` on the wire

**Status: done; see the commit log. The text below is what shipped.**

**Files:**
- Modify: `crates/scout-api/src/lib.rs` — after `Turn` (line ~168).
- Test: same file's tests module.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn a_thread_serialises_with_the_names_the_page_reads() {
        let t = Thread {
            id: 7,
            title: None,
            pinned: true,
            updated_at: "2026-09-05T10:00:00Z".to_string(),
            current: false,
        };
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json["id"], 7);
        assert!(json["title"].is_null());
        assert_eq!(json["pinned"], true);
        assert_eq!(json["updated_at"], "2026-09-05T10:00:00Z");
        assert_eq!(json["current"], false);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p scout-api -- a_thread_serialises`
Expected: compile error — `Thread` not found.

- [ ] **Step 3: Implement**

After `Turn`:

```rust
/// One thread in the browser's list. `current` is the one a Telegram
/// message would continue and the mirror follows; exactly one row has it
/// whenever the list is non-empty.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Thread {
    pub id: i64,
    /// Null until the first answer lands.
    pub title: Option<String>,
    pub pinned: bool,
    /// RFC 3339, UTC. A string so the page does not need a date library
    /// to show "2h ago".
    pub updated_at: String,
    pub current: bool,
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p scout-api`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-api/src/lib.rs
git commit -m "feat(api): a thread, as the page lists it

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 5: An automatic title after the first answer

**Status: done; see the commit log. The text below is what shipped.**

**Files:**
- Modify: `crates/scout-core/src/session.rs` — new pure fn `first_message_title`, new `title_if_missing`.
- Modify: `crates/scout-core/src/run.rs` — call it before `Ok(RunOutcome::Answered(reply))` (line ~278).
- Test: `session.rs` tests module; `run.rs` tests module.

- [ ] **Step 1: Write the failing tests**

In `session.rs` tests:

```rust
    #[test]
    fn a_title_is_the_first_message_on_one_line_cut_at_forty_characters() {
        assert_eq!(first_message_title("cheapest OneBlade cartridges"), "cheapest OneBlade cartridges");
        assert_eq!(
            first_message_title("  find me   the\ncheapest\n\nPhilips OneBlade replacement cartridges please "),
            "find me the cheapest Philips OneBlade re…"
        );
        // Cut on a character boundary, never inside one.
        assert_eq!(first_message_title(&"ë".repeat(50)), format!("{}…", "ë".repeat(40)));
        assert_eq!(first_message_title("   "), "");
    }

    #[tokio::test]
    async fn the_first_answer_names_the_thread_and_the_second_does_not_rename_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("titles.duckdb");
        let core = Core::start(crate::config::Config::for_test(path.to_str().unwrap()), None).unwrap();
        let store = core.store();
        let a = store.account_for_telegram(11).unwrap();
        let id = store.start_conversation(a, "direct").unwrap();

        title_if_missing(&core, id, "wasmiddel per kilo, bol.com").await;
        title_if_missing(&core, id, "only under 20 euro").await;

        assert_eq!(store.thread_title(a, id).unwrap().as_deref(), Some("wasmiddel per kilo, bol.com"));
    }
```

In `run.rs` tests:

```rust
    #[test]
    fn every_answered_run_names_a_thread_that_has_no_name_yet() {
        // Telegram never shows titles, so a thread started there would sit
        // nameless in the sidebar forever if only the web path titled it.
        // The one place both channels pass through is here.
        let src = include_str!("run.rs");
        let src = &src[..src.find("#[cfg(test)]").expect("the tests must come last")];
        let saved = src.find("save_history(").expect("the save must exist");
        let titled = src.find("title_if_missing(").expect("the title must be set");
        assert!(saved < titled, "the title is set before the history is saved");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p scout-core --lib -- session::tests::a_title_is session::tests::the_first_answer_names run::tests::every_answered_run_names`
Expected: compile errors in session (`first_message_title`, `title_if_missing` not found); the run.rs test fails on `expect("the title must be set")`.

- [ ] **Step 3: Implement**

In `session.rs`, above `#[cfg(test)]`:

```rust
/// How many characters of a first message become the thread's name.
const TITLE_CHARS: usize = 40;

/// The automatic name: the first message, whitespace collapsed to single
/// spaces, cut at `TITLE_CHARS` with an ellipsis when cut.
pub fn first_message_title(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = one_line.chars();
    let head: String = chars.by_ref().take(TITLE_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Names a thread after its first answer, unless it already has a name.
///
/// Called by `run_agent` on every answered run, so a thread begun on
/// Telegram is named too. Failure is logged, not returned: a missing title
/// is not worth failing an answer that is already written.
pub async fn title_if_missing(core: &Core, conversation_id: i64, prompt: &str) {
    let title = first_message_title(prompt);
    if title.is_empty() {
        return;
    }
    let store = core.store();
    if let Err(e) = crate::core::blocking(move || store.set_thread_title_if_missing(conversation_id, &title)).await {
        tracing::warn!(error = %e, conversation_id, "could not name the thread");
    }
}
```

In `run.rs`, replace the tail of `run_agent`:

```rust
    trim_history(&mut history, HISTORY_CAP);
    let store = core.deps.store.clone();
    if let Err(e) = crate::core::blocking(move || crate::session::save_history(&store, conversation_id, &history)).await {
        // The answer is already on its way to the user; losing the thread is
        // worse than not saving it, but it is not worth failing the reply.
        tracing::warn!(error = %e, conversation_id, "could not save the conversation");
    }
    // After the save, so a thread that failed to save is not named as if
    // it had. Writes only over a null title, so a rename survives.
    crate::session::title_if_missing(core, conversation_id, prompt).await;
    Ok(RunOutcome::Answered(reply))
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p scout-core --lib -- session::tests run::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/session.rs crates/scout-core/src/run.rs
git commit -m "feat: a thread is named after its first answer

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 6: Thread operations in `session.rs`

**Status: done; see the commit log. The text below is what shipped.**

**Files:**
- Modify: `crates/scout-core/src/session.rs` — after `current_thread`.
- Test: same tests module.

- [ ] **Step 1: Write the failing tests**

```rust
    async fn threads_core() -> (Core, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("threads.duckdb");
        let core = Core::start(crate::config::Config::for_test(path.to_str().unwrap()), None).unwrap();
        (core, dir)
    }

    #[tokio::test]
    async fn the_list_marks_exactly_the_thread_telegram_would_continue() {
        let (core, _dir) = threads_core().await;
        let a = core.store().account_for_telegram(11).unwrap();
        let older = seed_exchange_for_tests(&core, a, "direct", "beans", "three").await.unwrap();
        let newer = seed_exchange_for_tests(&core, a, "direct", "hubs", "two").await.unwrap();
        core.store().set_thread_pinned(a, older, true).unwrap();

        let list = threads(&core, a).await.unwrap();

        // Pinned first in the list, but current is by last use — so the
        // pinned row is first and the newer row is current.
        assert_eq!(list.iter().map(|t| t.id).collect::<Vec<_>>(), vec![older, newer]);
        assert_eq!(list.iter().filter(|t| t.current).count(), 1);
        assert!(list[1].current);
        assert_eq!(list[0].title.as_deref(), None, "seeding does not run the agent, so no title");
    }

    #[tokio::test]
    async fn opening_a_thread_makes_it_the_one_a_message_continues_and_returns_it() {
        let (core, _dir) = threads_core().await;
        let a = core.store().account_for_telegram(11).unwrap();
        let older = seed_exchange_for_tests(&core, a, "direct", "beans", "three").await.unwrap();
        let _newer = seed_exchange_for_tests(&core, a, "direct", "hubs", "two").await.unwrap();

        let turns = open_thread(&core, a, older).await.unwrap().expect("my own thread");

        assert_eq!(turns[0].text, "beans");
        assert_eq!(latest_direct(&core.store(), a).unwrap(), Some(older));
        assert_eq!(open_thread(&core, a, 424242).await.unwrap(), None, "opened nothing");
    }

    #[tokio::test]
    async fn rename_pin_and_delete_answer_not_found_for_someone_elses_thread() {
        let (core, _dir) = threads_core().await;
        let a = core.store().account_for_telegram(11).unwrap();
        let b = core.store().account_for_telegram(22).unwrap();
        let mine = seed_exchange_for_tests(&core, a, "direct", "beans", "three").await.unwrap();

        assert!(!rename(&core, b, mine, "theirs").await.unwrap());
        assert!(!set_pinned(&core, b, mine, true).await.unwrap());
        assert!(!delete_thread(&core, b, mine).await.unwrap());

        assert!(rename(&core, a, mine, "  beans, cheapest  ").await.unwrap());
        assert_eq!(core.store().thread_title(a, mine).unwrap().as_deref(), Some("beans, cheapest"));
        assert!(set_pinned(&core, a, mine, true).await.unwrap());
        assert!(threads(&core, a).await.unwrap()[0].pinned);
        assert!(delete_thread(&core, a, mine).await.unwrap());
        assert!(threads(&core, a).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_rename_refuses_an_empty_name_and_cuts_a_long_one() {
        let (core, _dir) = threads_core().await;
        let a = core.store().account_for_telegram(11).unwrap();
        let mine = seed_exchange_for_tests(&core, a, "direct", "beans", "three").await.unwrap();

        assert!(rename(&core, a, mine, "   ").await.is_err(), "an empty name is not a name");
        rename(&core, a, mine, &"x".repeat(100)).await.unwrap();
        assert_eq!(core.store().thread_title(a, mine).unwrap().unwrap().chars().count(), 80);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p scout-core --lib -- session::tests::the_list_marks session::tests::opening_a_thread session::tests::rename_pin session::tests::a_rename_refuses`
Expected: compile errors — `threads`, `open_thread`, `rename`, `set_pinned`, `delete_thread` not found.

- [ ] **Step 3: Implement**

After `current_thread` in `session.rs`:

```rust
/// The longest a person may make a title.
const RENAME_CHARS: usize = 80;

/// The account's threads for the sidebar: pinned first, then by last use,
/// with `current` on the one `latest_direct` returns.
pub async fn threads(core: &Core, account_id: i64) -> anyhow::Result<Vec<scout_api::Thread>> {
    let store = core.store();
    crate::core::blocking(move || {
        let current = latest_direct(&store, account_id)?;
        Ok(store
            .threads_of(account_id)?
            .into_iter()
            .map(|row| scout_api::Thread {
                current: Some(row.id) == current,
                id: row.id,
                title: row.title,
                pinned: row.pinned,
                updated_at: row.updated_at,
            })
            .collect())
    })
    .await
}

/// Switches to a thread: bumps it to current and returns its transcript.
/// `None` when the account has no such thread.
pub async fn open_thread(
    core: &Core,
    account_id: i64,
    conversation_id: i64,
) -> anyhow::Result<Option<Vec<scout_api::Turn>>> {
    let store = core.store();
    crate::core::blocking(move || {
        if !store.open_conversation(account_id, conversation_id)? {
            return Ok(None);
        }
        Ok(Some(transcript_of(&store, conversation_id)?))
    })
    .await
}

/// A name the person chose. Trimmed, refused when empty, cut at
/// `RENAME_CHARS`. `false` when the thread is not theirs.
pub async fn rename(core: &Core, account_id: i64, conversation_id: i64, title: &str) -> anyhow::Result<bool> {
    let title: String = title.trim().chars().take(RENAME_CHARS).collect();
    if title.is_empty() {
        anyhow::bail!("a thread needs a name");
    }
    let store = core.store();
    crate::core::blocking(move || store.set_thread_title(account_id, conversation_id, &title)).await
}

pub async fn set_pinned(core: &Core, account_id: i64, conversation_id: i64, pinned: bool) -> anyhow::Result<bool> {
    let store = core.store();
    crate::core::blocking(move || store.set_thread_pinned(account_id, conversation_id, pinned)).await
}

pub async fn delete_thread(core: &Core, account_id: i64, conversation_id: i64) -> anyhow::Result<bool> {
    let store = core.store();
    crate::core::blocking(move || store.delete_conversation(account_id, conversation_id)).await
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p scout-core --lib -- session::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/session.rs
git commit -m "feat: list, open, rename, pin and delete a thread

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 7: A model-suggested title

**Status: done; see the commit log. The text below is what shipped.**

**Files:**
- Modify: `crates/scout-core/src/agent.rs` — `title_for` beside `continues_previous` (line ~345).
- Modify: `crates/scout-core/src/session.rs` — `clean_title` pure fn and `suggest_title`.
- Test: `session.rs` tests.

The model call itself cannot be tested (no live model in the workspace, same as `continues_previous`). What can be is what happens to its answer.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn a_suggested_title_is_one_line_without_quotes_and_never_empty() {
        assert_eq!(clean_title("\"Cheapest OneBlade cartridges\"\n"), Some("Cheapest OneBlade cartridges".to_string()));
        assert_eq!(clean_title("Title: 'AMS to LIS in October'."), Some("AMS to LIS in October".to_string()));
        assert_eq!(clean_title("<think>hmm</think>Wasmiddel per kilo"), Some("Wasmiddel per kilo".to_string()));
        assert_eq!(clean_title("   "), None);
        assert_eq!(clean_title(&"word ".repeat(30)).unwrap().chars().count(), 80);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p scout-core --lib -- session::tests::a_suggested_title`
Expected: compile error — `clean_title` not found.

- [ ] **Step 3: Implement**

In `agent.rs`, after `continues_previous`:

```rust
/// One-shot: a short name for a conversation, from its text. Tool-less,
/// like `continues_previous`, and for the same reason.
pub async fn title_for(llm: &LlmClient, transcript: &str) -> Result<String> {
    let agent = llm
        .agent(MODEL)
        .preamble(
            "You name chat threads. Reply with a title of at most five words \
             in the language of the conversation, no quotes, no trailing \
             punctuation, nothing else.",
        )
        .build();
    let question = format!("Conversation:\n{transcript}\n\nTitle:");
    Ok(crate::text::strip_thinking(&rig::completion::Prompt::prompt(&agent, question).await?))
}
```

In `session.rs`, after `delete_thread`:

```rust
/// What the model said, made fit for a sidebar: first non-empty line, a
/// leading "Title:" and wrapping quotes dropped, trailing punctuation
/// dropped, cut at `RENAME_CHARS`. `None` when nothing is left.
pub fn clean_title(raw: &str) -> Option<String> {
    let line = crate::text::strip_thinking(raw);
    let line = line.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line.strip_prefix("Title:").map(str::trim).unwrap_or(line);
    let line = line.trim_matches(|c: char| c == '"' || c == '\'' || c == '“' || c == '”' || c == '‘' || c == '’');
    let line = line.trim_end_matches(|c: char| c == '.' || c == '!' || c == '?' || c == ':');
    let cleaned: String = line.trim().chars().take(RENAME_CHARS).collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Asks the model for a name and stores it. `None` when the thread is not
/// the account's, has nothing in it to name, or went away while the model
/// was thinking. An unusable answer is an error the caller reports; the old
/// title stays.
///
/// The thread is read, not opened: naming an old sidebar row must not make
/// it the thread Telegram continues, which is what `open_thread` would do.
pub async fn suggest_title(core: &Core, account_id: i64, conversation_id: i64) -> anyhow::Result<Option<String>> {
    let store = core.store();
    let Some(turns) = crate::core::blocking(move || {
        if !store.owns_thread(account_id, conversation_id)? {
            return Ok(None);
        }
        Ok(Some(transcript_of(&store, conversation_id)?))
    })
    .await?
    else {
        return Ok(None);
    };
    if turns.is_empty() {
        return Ok(None);
    }
    let text: String = turns
        .iter()
        .take(6)
        .map(|t| format!("{}: {}", match t.role { scout_api::Role::You => "user", scout_api::Role::Scout => "scout" }, t.text.chars().take(400).collect::<String>()))
        .collect::<Vec<_>>()
        .join("\n");
    let raw = crate::agent::title_for(&core.deps.llm, &text).await?;
    let Some(title) = clean_title(&raw) else {
        anyhow::bail!("the model gave no usable title");
    };
    let store = core.store();
    let written = title.clone();
    // The thread can be deleted while the model is thinking; then nothing
    // was named, and saying otherwise would put a title on a gone row.
    let stored = crate::core::blocking(move || store.set_thread_title(account_id, conversation_id, &written)).await?;
    Ok(stored.then_some(title))
}
```

Note: `owns_thread` is the ownership check on its own — the same predicate `open_conversation` uses, without the bump. Naming a thread must not switch to it: a person naming an old row does not expect their phone to continue it. And the write's own `bool` is the answer, not a discarded one: the row can go while the model is thinking, and `Some(title)` for a thread that no longer exists is a lie the caller would show.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p scout-core --lib -- session::tests`
Expected: PASS. Also `cargo clippy -p scout-core` clean.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/agent.rs crates/scout-core/src/session.rs
git commit -m "feat: the model can name a thread on request

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 8: Expiry in maintenance

**Status: done; see the commit log. The text below is what shipped.**

**Files:**
- Modify: `crates/scout-core/src/core.rs` — `run_maintenance` (line ~457) and a private helper beside `prune_login_tokens`.
- Test: `core.rs` tests module.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn maintenance_actually_expires_threads() {
        // The store method is tested on its own; nothing else says it is
        // ever called. A forgotten call leaves every thread forever and
        // the sidebar growing without bound.
        let src = include_str!("core.rs");
        let src = &src[..src.find("#[cfg(test)]").expect("the tests must come last")];
        let start = src.find("pub async fn run_maintenance").expect("the loop must exist");
        let end = src[start..].find("\n    }").expect("the loop must end") + start;
        let body = &src[start..end];
        assert!(body.contains("expire_threads("), "idle threads are never expired");
        // And above the backup's `continue`: a `Ok(false) => continue` sits
        // between them, so an expiry placed after it would only run on the
        // one tick a day that a backup is due.
        let expiry = body.find("expire_threads(").unwrap();
        let backup = body.find("backup::is_due").expect("the backup check must exist");
        assert!(expiry < backup, "expiry sits below the backup's continue and would run once a day");
    }
```

The slice must end at the function, not run to the end of the file: the
private `expire_threads` helper below `run_maintenance` contains the same
text, so an unbounded body would find the definition and call the call
proven.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p scout-core --lib -- core::tests::maintenance_actually_expires`
Expected: FAIL — "idle threads are never expired".

- [ ] **Step 3: Implement**

In `run_maintenance`, after the `prune_login_tokens` match:

```rust
            // The other thing that grows on its own. Two days idle and not
            // pinned, any scope — see the threads design doc.
            match self.expire_threads().await {
                Ok(0) => {}
                Ok(n) => tracing::info!(expired = n, "idle threads dropped"),
                Err(e) => tracing::warn!(error = %e, "could not expire idle threads"),
            }
```

Beside `prune_login_tokens`:

```rust
    /// How long a thread may sit untouched before it goes, unless pinned.
    const THREAD_IDLE_SECS: i64 = 48 * 3600;

    async fn expire_threads(&self) -> anyhow::Result<usize> {
        let store = self.store();
        blocking(move || store.expire_conversations(Self::THREAD_IDLE_SECS)).await
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p scout-core --lib -- core::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/core.rs
git commit -m "feat: idle threads expire hourly

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 9: A divider the mirror sends on a switch

**Files:**
- Modify: `crates/scout-core/src/mirror.rs` — `note` after `enqueue`.
- Test: `mirror.rs` tests module.

The existing turn key is `hash(conversation_id, role, text)`, which makes a backfill idempotent. A switch divider must not be idempotent — switching to a thread twice is two events — so its key carries the moment.

- [ ] **Step 1: Write the failing test**

Look at the top of `mirror.rs`'s tests module for how a `Core` is built there (it uses `Core::start(Config::for_test(...))` on a temp path; reuse that helper by name — it is called `test_core` if present; if not, define one exactly as in `session.rs`'s `threads_core`). Then:

```rust
    #[tokio::test]
    async fn a_note_is_queued_every_time_not_once_per_wording() {
        let (core, _dir) = test_core().await;
        let a = core.store().account_for_telegram(11).unwrap();

        assert_eq!(note(&core, a, "12345", "── beans ──", 1_000).await.unwrap(), 1);
        assert_eq!(note(&core, a, "12345", "── beans ──", 1_001).await.unwrap(), 1, "the second switch was swallowed");

        let pending = pending(&core, 10).await.unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().all(|p| p.body == "── beans ──"));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p scout-core --lib -- mirror::tests::a_note_is_queued`
Expected: compile error — `note` not found.

- [ ] **Step 3: Implement**

After `enqueue`:

```rust
/// One line to the phone that is an event, not a turn: "you switched to
/// this thread". Keyed on `at` (Unix seconds) as well as the text, so the
/// same switch made twice is sent twice, and a double-click inside one
/// second is sent once.
pub async fn note(core: &Core, account_id: i64, address: &str, text: &str, at: i64) -> anyhow::Result<usize> {
    let store = core.store();
    let address = address.to_string();
    let key = turn_key(-at, Role::Scout, text);
    let body = text.to_string();
    let written = crate::core::blocking(move || {
        Ok(usize::from(store.enqueue_mirror(account_id, TELEGRAM, &address, &body, &key, false)?))
    })
    .await?;
    if written > 0 {
        core.wake_mirror();
    }
    Ok(written)
}
```

(`-at` is negative so it can never collide with a real conversation id.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p scout-core --lib -- mirror::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/mirror.rs
git commit -m "feat(mirror): a note that is sent every time it happens

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 10: The thread routes

**Files:**
- Modify: `crates/scout-web/src/routes/chat.rs` — `routes()`, `MessageIn`, `send_message`, new handlers.
- Test: same file's tests module (helpers `test_app_with_a_round`, `admitted`, `seed_conversation`, `get_with_cookie`, `post_json_with_cookie`, `body_of`, `TEST_KEY`, `DAY` already exist there).

- [ ] **Step 1: Write the failing tests**

```rust
    async fn threads_json(app: &axum::Router, cookie: &str) -> serde_json::Value {
        serde_json::from_str(&body_of(get_with_cookie(app, "/chat/threads", cookie).await).await).unwrap()
    }

    #[tokio::test]
    async fn the_thread_list_is_the_accounts_direct_threads_with_one_current() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        seed_conversation(&core, account_id, "beans", "three").await;
        seed_conversation(&core, account_id, "hubs", "two").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);

        let list = threads_json(&app, &cookie).await;
        let list = list.as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list.iter().filter(|t| t["current"] == true).count(), 1);
        assert!(list[0]["current"] == true, "newest is current and first: {list:?}");
    }

    #[tokio::test]
    async fn opening_a_thread_returns_its_transcript_and_makes_it_current() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let older = scout_core::session::seed_exchange_for_tests(&core, account_id, "direct", "beans", "three").await.unwrap();
        seed_conversation(&core, account_id, "hubs", "two").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);

        let res = post_json_with_cookie(&app, &format!("/chat/threads/{older}/open"), &cookie, Some(&csrf), "").await;
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_of(res).await;
        assert!(body.contains("beans"), "got: {body}");

        let list = threads_json(&app, &cookie).await;
        assert_eq!(list[0]["id"], older);
        assert_eq!(list[0]["current"], true);
    }

    #[tokio::test]
    async fn someone_elses_thread_is_not_found_on_every_route() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let owner = admitted(&core, "777").await;
        let theirs = scout_core::session::seed_exchange_for_tests(&core, owner, "direct", "beans", "three").await.unwrap();
        let me = admitted(&core, "888").await;
        let cookie = crate::session::mint(TEST_KEY, me, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, me);

        for (path, body) in [
            ("open", ""),
            ("rename", r#"{"title":"mine"}"#),
            ("pin", r#"{"pinned":true}"#),
            ("delete", ""),
        ] {
            let res = post_json_with_cookie(&app, &format!("/chat/threads/{theirs}/{path}"), &cookie, Some(&csrf), body).await;
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path} answered {}", res.status());
        }
        assert_eq!(
            scout_core::session::transcript(&core, owner).await.unwrap().len(),
            2,
            "a stranger changed the owner's thread"
        );
    }

    #[tokio::test]
    async fn rename_pin_and_delete_change_what_the_list_says() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let id = scout_core::session::seed_exchange_for_tests(&core, account_id, "direct", "beans", "three").await.unwrap();
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);

        let res = post_json_with_cookie(&app, &format!("/chat/threads/{id}/rename"), &cookie, Some(&csrf), r#"{"title":"cheapest beans"}"#).await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let res = post_json_with_cookie(&app, &format!("/chat/threads/{id}/pin"), &cookie, Some(&csrf), r#"{"pinned":true}"#).await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let list = threads_json(&app, &cookie).await;
        assert_eq!(list[0]["title"], "cheapest beans");
        assert_eq!(list[0]["pinned"], true);

        let res = post_json_with_cookie(&app, &format!("/chat/threads/{id}/delete"), &cookie, Some(&csrf), "").await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert!(threads_json(&app, &cookie).await.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_empty_rename_is_refused() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let id = scout_core::session::seed_exchange_for_tests(&core, account_id, "direct", "beans", "three").await.unwrap();
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);
        let res = post_json_with_cookie(&app, &format!("/chat/threads/{id}/rename"), &cookie, Some(&csrf), r#"{"title":"  "}"#).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_new_thread_is_returned_as_a_thread_and_is_current() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        seed_conversation(&core, account_id, "beans", "three").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, account_id);

        let res = post_json_with_cookie(&app, "/chat/threads", &cookie, Some(&csrf), "").await;
        assert_eq!(res.status(), StatusCode::OK);
        let thread: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_eq!(thread["current"], true);
        assert!(thread["title"].is_null());
        assert_eq!(threads_json(&app, &cookie).await.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_message_into_a_thread_that_is_not_yours_is_refused_before_anything_runs() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let owner = admitted(&core, "777").await;
        let theirs = scout_core::session::seed_exchange_for_tests(&core, owner, "direct", "beans", "three").await.unwrap();
        let me = admitted(&core, "888").await;
        let cookie = crate::session::mint(TEST_KEY, me, DAY);
        let csrf = crate::session::csrf_for(TEST_KEY, me);

        let res = post_json_with_cookie(
            &app, "/chat/messages", &cookie, Some(&csrf), &format!(r#"{{"text":"hi","thread":{theirs}}}"#),
        ).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn a_message_runs_on_the_thread_the_page_named_not_on_whatever_is_newest() {
        // `run_agent` needs a live model, so the join is asserted from the
        // source: the handler must open the named thread and must not fall
        // back to `resolve_conversation`, which picks the newest — the
        // race this field exists to close.
        let src = include_str!("chat.rs");
        let start = src.find("async fn send_message").expect("the handler must exist");
        let end = src[start..].find("\n}\n").expect("the handler must end") + start;
        let body = &src[start..end];
        assert!(body.contains("open_thread("), "the message does not go to the named thread");
        assert!(!body.contains("resolve_conversation("), "the message can still land in the newest thread");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p scout-web --lib -- routes::chat::tests::the_thread_list routes::chat::tests::opening_a_thread routes::chat::tests::someone_elses routes::chat::tests::rename_pin routes::chat::tests::an_empty_rename routes::chat::tests::a_new_thread routes::chat::tests::a_message_into routes::chat::tests::a_message_runs`
Expected: the source-scan test FAILs on "does not go to the named thread"; the HTTP tests FAIL with 404 from the router (route missing) or, for `/chat/messages`, a non-404 status.

- [ ] **Step 3: Implement**

In `routes()`:

```rust
        .route("/chat/threads", get(list_threads).post(new_thread))
        .route("/chat/threads/{id}/open", post(open_thread))
        .route("/chat/threads/{id}/rename", post(rename_thread))
        .route("/chat/threads/{id}/pin", post(pin_thread))
        .route("/chat/threads/{id}/delete", post(delete_thread))
        .route("/chat/threads/{id}/title", post(suggest_title))
```

Change `MessageIn`:

```rust
#[derive(serde::Deserialize)]
struct MessageIn {
    text: String,
    /// The thread the page is showing. Named rather than inferred: Telegram
    /// may have started a newer thread while the page sat open, and a
    /// message would otherwise land there.
    thread: i64,
}
```

In `send_message`, replace the `resolve_conversation` block with:

```rust
    // Opening it is the ownership check and the bump to current, so the
    // run happens on exactly the thread the reader is looking at.
    let conversation_id = match scout_core::session::open_thread(&auth.core, account_id, body.thread).await {
        Ok(Some(_)) => body.thread,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "could not open a conversation");
            return sorry();
        }
    };
```

Handlers, placed after `reset`:

```rust
/// The account and the CSRF check every thread route starts with.
async fn thread_caller(auth: &AuthState, headers: &HeaderMap) -> Result<i64, Response> {
    let account_id = admitted_account(auth, headers).await?;
    if !csrf_header_ok(auth, headers, account_id) {
        return Err(StatusCode::BAD_REQUEST.into_response());
    }
    Ok(account_id)
}

async fn list_threads(axum::extract::State(auth): axum::extract::State<AuthState>, headers: HeaderMap) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    match scout_core::session::threads(&auth.core, account_id).await {
        Ok(list) => axum::Json(list).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "could not list threads");
            sorry()
        }
    }
}

async fn new_thread(axum::extract::State(auth): axum::extract::State<AuthState>, headers: HeaderMap) -> Response {
    let account_id = match thread_caller(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    mirror_divider(&auth, account_id).await;
    let id = match scout_core::session::reset(&auth.core, account_id, "direct").await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "could not start a thread");
            return sorry();
        }
    };
    match scout_core::session::threads(&auth.core, account_id).await {
        Ok(list) => match list.into_iter().find(|t| t.id == id) {
            Some(thread) => axum::Json(thread).into_response(),
            None => sorry(),
        },
        Err(e) => {
            tracing::error!(error = %e, "could not read the new thread back");
            sorry()
        }
    }
}

async fn open_thread(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    headers: HeaderMap,
) -> Response {
    let account_id = match thread_caller(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    match scout_core::session::open_thread(&auth.core, account_id, id).await {
        Ok(Some(turns)) => {
            mirror_switch_note(&auth, account_id, id).await;
            axum::Json(turns).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "could not open a thread");
            sorry()
        }
    }
}

#[derive(serde::Deserialize)]
struct RenameIn {
    title: String,
}

async fn rename_thread(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<RenameIn>,
) -> Response {
    let account_id = match thread_caller(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    // No `trim().is_empty()` pre-check here. `session::rename` already
    // decides what counts as a name — it trims, strips layout characters
    // and cuts — and a handler with its own idea of "blank" would disagree
    // with it the first time that rule grows: `"\u{202E}"` is not empty to
    // a `trim()`, and is nothing at all by the time it is stored.
    match scout_core::session::rename(&auth.core, account_id, id, &body.title).await {
        Ok(scout_core::session::Renamed::Done) => StatusCode::NO_CONTENT.into_response(),
        Ok(scout_core::session::Renamed::NotFound) => StatusCode::NOT_FOUND.into_response(),
        Ok(scout_core::session::Renamed::Blank) => StatusCode::BAD_REQUEST.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "could not rename a thread");
            sorry()
        }
    }
}

#[derive(serde::Deserialize)]
struct PinIn {
    pinned: bool,
}

async fn pin_thread(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<PinIn>,
) -> Response {
    let account_id = match thread_caller(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    match scout_core::session::set_pinned(&auth.core, account_id, id, body.pinned).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "could not pin a thread");
            sorry()
        }
    }
}

async fn delete_thread(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    headers: HeaderMap,
) -> Response {
    let account_id = match thread_caller(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    match scout_core::session::delete_thread(&auth.core, account_id, id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "could not delete a thread");
            sorry()
        }
    }
}

#[derive(serde::Serialize)]
struct TitleOut {
    title: String,
}

async fn suggest_title(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    headers: HeaderMap,
) -> Response {
    let account_id = match thread_caller(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    match scout_core::session::suggest_title(&auth.core, account_id, id).await {
        Ok(Some(title)) => axum::Json(TitleOut { title }).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "could not suggest a title");
            sorry()
        }
    }
}

/// Tells the phone which thread the browser switched to, when the mirror
/// is on. A note rather than a backfill: the whole thread is a tap away
/// on the laptop, and twenty paced messages is not a heads-up.
async fn mirror_switch_note(auth: &AuthState, account_id: i64, conversation_id: i64) {
    if !matches!(scout_core::mirror::is_enabled(&auth.core, account_id).await, Ok(true)) {
        return;
    }
    let Some(reply_to) = reply_to_for(auth, account_id).await else {
        return;
    };
    let title = match scout_core::session::threads(&auth.core, account_id).await {
        Ok(list) => list.into_iter().find(|t| t.id == conversation_id).and_then(|t| t.title),
        Err(_) => None,
    };
    let text = format!("── {} ──", title.unwrap_or_else(|| "New thread".to_string()));
    let at = chrono::Utc::now().timestamp();
    if let Err(e) = scout_core::mirror::note(&auth.core, account_id, &reply_to.address, &text, at).await {
        tracing::warn!(error = %e, account_id, "could not note the switch for Telegram");
    }
}
```

Keep the `/chat/reset` route and its handler as they are (the spec keeps it as an alias).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p scout-web`
Expected: PASS. Existing tests that POST `/chat/messages` with `{"text":"hi"}` and expect a CSRF or origin refusal still pass because those checks run before the body is used — verify; if one now fails with 422 (missing `thread`), add `"thread":1` to its body.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-web/src/routes/chat.rs
git commit -m "feat(web): routes for a thread list, and a message that names its thread

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 11: The sidebar on the page

**Files:**
- Modify: `crates/scout-web/src/chat.html`.
- Modify: `crates/scout-web/src/routes/chat.rs` — the id list in `the_page_still_carries_every_id_the_client_binds_to` (line ~811).

- [ ] **Step 1: Extend the failing test**

Change the id list to:

```rust
        for id in ["turns", "status", "notice", "ask", "text", "send", "reset", "threads", "menu", "side"] {
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p scout-web --lib -- the_page_still_carries_every_id`
Expected: FAIL on `#threads`.

- [ ] **Step 3: Rewrite the markup and CSS**

Replace `<body>` … `</body>` in `chat.html` with:

```html
<body>
<div class="wrap">
<header>
  <button id="menu" type="button" class="menu" aria-label="Threads" aria-expanded="false">
    <svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true">
      <path d="M4 7h16M4 12h16M4 17h16" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round"/>
    </svg>
  </button>
  <a class="mark" href="/">
    <svg class="logo" viewBox="0 0 72 72" aria-hidden="true">
      <g fill="none" stroke="currentColor">
        <circle cx="23.5" cy="36" r="11.5" stroke-width="3.2"/>
        <circle cx="48.5" cy="36" r="11.5" stroke-width="3.2"/>
        <circle cx="23.5" cy="36" r="4" stroke-width="2.6"/>
        <circle cx="48.5" cy="36" r="4" stroke-width="2.6"/>
      </g>
      <rect x="34" y="33.4" width="4" height="5.2" fill="currentColor"/>
    </svg>
    <span class="word">Scout</span>
  </a>
  <div class="controls">
    <button id="mirror" type="button" aria-pressed="<!--MIRROR-->"
            title="Also send this thread to Telegram">
      <svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true">
        <path fill="currentColor" d="M21.7 3.4 2.9 10.6c-.9.3-.9 1.5.05 1.8l4.7 1.5 1.8 5.3c.3.8 1.3 1 1.9.4l2.6-2.5 4.6 3.4c.6.4 1.5.1 1.7-.7l3.2-15c.2-.9-.7-1.6-1.75-1.4z"/>
      </svg>
    </button>
    <form id="reset" class="reset">
      <button type="submit">New thread</button>
    </form>
  </div>
</header>
<div class="columns">
  <aside id="side" class="side">
    <ol id="threads" class="threads"></ol>
  </aside>
  <main class="main">
    <ol id="turns" class="turns"></ol>
    <p id="status" class="status" hidden></p>
    <p id="notice" class="notice" hidden></p>
    <form id="ask" class="ask">
      <textarea id="text" name="text" rows="1"
                placeholder="Ask Scout something" required></textarea>
      <button id="send" type="submit" aria-label="Send">
        <svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true">
          <path d="M12 19V5M5 12l7-7 7 7" fill="none" stroke="currentColor"
                stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/>
        </svg>
      </button>
    </form>
  </main>
</div>
</div>
<script type="module" src="/chat.js"></script>
</body>
```

Add to the `<style>` block, after the existing `#mirror[aria-pressed="true"]` rule:

```css
  /* Two columns above 720px, one below. `.wrap` is capped at 720px for a
     single column of text; with a sidebar it may be wider. */
  .wrap{max-width:1040px}
  .columns{flex:1; min-height:0; display:grid; grid-template-columns:240px 1fr; gap:24px}
  .main{display:flex; flex-direction:column; min-height:0}
  .side{min-height:0; overflow-y:auto; padding:4px 0 16px; border-right:1px solid #0d4a5a}
  .threads{list-style:none; margin:0; padding:0 12px 0 0; display:flex; flex-direction:column; gap:2px}
  .threads li{display:flex; align-items:center; gap:6px; padding:8px 10px; border-radius:10px;
    font-size:14px; color:var(--base0); cursor:pointer; min-width:0}
  .threads li:hover{background:var(--base02)}
  .threads li.current{background:var(--base02); color:var(--base2)}
  .threads .title{flex:1; min-width:0; overflow:hidden; text-overflow:ellipsis; white-space:nowrap}
  .threads .title.unnamed{color:var(--base01); font-style:italic}
  .threads .title input{width:100%; font:inherit; background:var(--base03); color:var(--base2);
    border:1px solid var(--blue); border-radius:6px; padding:2px 6px}
  .threads .when{font-size:12px; color:var(--base01); flex:none}
  .threads .when.expiring{color:var(--red)}
  .threads .pin{flex:none; font-size:12px; color:var(--blue)}
  /* Controls show on hover and on the current row. Touch has no hover, so
     the current row is the row a phone can act on. */
  .threads .tools{display:none; gap:2px; flex:none}
  .threads li:hover .tools,.threads li.current .tools{display:flex}
  .threads .tools button{background:transparent; border:0; color:var(--base01); cursor:pointer;
    padding:2px 4px; font-size:13px; line-height:1}
  .threads .tools button:hover{color:var(--base1)}
  .threads .tools button[aria-pressed="true"]{color:var(--blue)}
  .menu{display:none; background:transparent; border:0; color:var(--base1); cursor:pointer; padding:4px}
  @media (max-width:720px){
    .columns{display:flex; flex-direction:column; gap:0}
    .side{display:none; border-right:0; border-bottom:1px solid #0d4a5a; max-height:40dvh; padding:0 0 8px}
    .side.open{display:block}
    .threads{padding:0}
    .menu{display:block}
  }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p scout-web`
Expected: PASS, including `the_page_without_a_mirror_toggle_is_still_a_whole_page`, the long-url test and the narration test (they scan CSS; if one asserts a `.wrap{max-width:720px}` literal, keep that original rule and let the later `.wrap{max-width:1040px}` override it in cascade order).

- [ ] **Step 5: Commit**

```bash
git add crates/scout-web/src/chat.html crates/scout-web/src/routes/chat.rs
git commit -m "feat(web): a sidebar for threads, a drawer on a phone

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 12: Pure client functions

**Files:**
- Modify: `crates/scout-web/src/chat.js` — exports after `composerHeight`.
- Test: `crates/scout-web/src/chat.test.mjs`.

- [ ] **Step 1: Write the failing tests**

Add to the import line: `threadLabel, whenLabel, sendBody`. Append:

```js
test('a thread is labelled by its title, or as new when it has none', () => {
  assert.deepEqual(threadLabel({ title: 'wasmiddel per kilo' }), { text: 'wasmiddel per kilo', unnamed: false })
  assert.deepEqual(threadLabel({ title: null }), { text: 'New thread', unnamed: true })
})

test('a thread says when it was last used, and when it is about to go', () => {
  const now = Date.parse('2026-09-05T12:00:00Z')
  assert.deepEqual(whenLabel({ updated_at: '2026-09-05T11:58:00Z', pinned: false }, now), { text: 'now', expiring: false })
  assert.deepEqual(whenLabel({ updated_at: '2026-09-05T09:30:00Z', pinned: false }, now), { text: '2h', expiring: false })
  assert.deepEqual(whenLabel({ updated_at: '2026-09-04T00:00:00Z', pinned: false }, now), { text: 'expires in 12h', expiring: true })
  // Pinned never expires, however old.
  assert.deepEqual(whenLabel({ updated_at: '2026-09-01T00:00:00Z', pinned: true }, now), { text: '4d', expiring: false })
})

test('a message names the thread it belongs to', () => {
  assert.deepEqual(JSON.parse(sendBody('hi', 42)), { text: 'hi', thread: 42 })
})
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `node --test 'crates/scout-web/src/*.test.mjs'`
Expected: FAIL — import of `threadLabel` etc. is undefined (SyntaxError on missing export).

- [ ] **Step 3: Implement**

After `composerHeight` in `chat.js`:

```js
// The idle window after which an unpinned thread is deleted, and the point
// at which the sidebar starts saying so. Both mirror core: 48h expiry in
// `Core::THREAD_IDLE_SECS`, and "worth warning" at 36h.
const EXPIRES_AFTER_MS = 48 * 3600 * 1000
const WARN_AFTER_MS = 36 * 3600 * 1000

export function threadLabel(thread) {
  return thread.title ? { text: thread.title, unnamed: false } : { text: 'New thread', unnamed: true }
}

// "2h", "4d", or "expires in 12h" once an unpinned thread is close to
// going — so nobody learns about expiry by losing something.
export function whenLabel(thread, now = Date.now()) {
  const age = Math.max(0, now - Date.parse(thread.updated_at))
  if (!thread.pinned && age >= WARN_AFTER_MS) {
    const left = Math.max(1, Math.ceil((EXPIRES_AFTER_MS - age) / 3600000))
    return { text: `expires in ${left}h`, expiring: true }
  }
  const minutes = Math.floor(age / 60000)
  if (minutes < 5) return { text: 'now', expiring: false }
  if (minutes < 60) return { text: `${minutes}m`, expiring: false }
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return { text: `${hours}h`, expiring: false }
  return { text: `${Math.floor(hours / 24)}d`, expiring: false }
}

export function sendBody(text, thread) {
  return JSON.stringify({ text, thread })
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `node --test 'crates/scout-web/src/*.test.mjs'`
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-web/src/chat.js crates/scout-web/src/chat.test.mjs
git commit -m "feat(web): what a thread row says, tested without a browser

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 13: Wiring the sidebar

**Files:**
- Modify: `crates/scout-web/src/chat.js` — inside `start()`.

No unit test covers DOM wiring (none does today). The route tests and the id test are the net; verify by hand in Step 3.

- [ ] **Step 1: Replace `loadHistory`, the reset handler and the send body**

Inside `start()`, after the existing element lookups add:

```js
  const sideEl = document.getElementById('side')
  const threadsEl = document.getElementById('threads')
  const menuButton = document.getElementById('menu')
  // The thread the page is showing. Every message names it, so a thread
  // the phone started meanwhile cannot swallow a message meant for this one.
  let currentThread = null
```

Replace `loadHistory` with:

```js
  async function loadHistory() {
    const res = await fetch('/chat/history')
    if (!res.ok) {
      showNotice('Could not load the conversation so far. Reload to try again.')
      return
    }
    showTurns(await res.json())
    await refreshThreads()
  }

  function showTurns(turns) {
    turnsEl.replaceChildren()
    for (const turn of turns) {
      turnsEl.append(turnElement(turn.role, turn.text))
    }
    turnsEl.scrollTop = turnsEl.scrollHeight
  }

  async function post(path, body) {
    return fetch(path, {
      method: 'POST',
      headers: { 'content-type': 'application/json', 'x-scout-csrf': csrfToken },
      body: body === undefined ? undefined : JSON.stringify(body),
    })
  }

  async function refreshThreads() {
    const res = await fetch('/chat/threads')
    if (!res.ok) return
    const list = await res.json()
    const current = list.find((t) => t.current)
    currentThread = current ? current.id : null
    renderThreads(list)
  }

  function renderThreads(list) {
    threadsEl.replaceChildren()
    for (const thread of list) {
      threadsEl.append(threadRow(thread))
    }
  }

  function threadRow(thread) {
    const li = document.createElement('li')
    li.dataset.id = thread.id
    if (thread.current) li.classList.add('current')

    const label = threadLabel(thread)
    const title = document.createElement('span')
    title.className = label.unnamed ? 'title unnamed' : 'title'
    title.textContent = label.text
    li.append(title)

    if (thread.pinned) {
      const pin = document.createElement('span')
      pin.className = 'pin'
      pin.textContent = '📌'
      li.append(pin)
    }

    const when = whenLabel(thread)
    const whenEl = document.createElement('span')
    whenEl.className = when.expiring ? 'when expiring' : 'when'
    whenEl.textContent = when.text
    li.append(whenEl)

    const tools = document.createElement('span')
    tools.className = 'tools'
    tools.append(
      toolButton('📌', thread.pinned ? 'Unpin' : 'Pin', () => pinThread(thread), thread.pinned),
      toolButton('✎', 'Rename', () => renameInline(li, title, thread)),
      toolButton('✦', 'Ask Scout for a name', () => suggestTitle(thread)),
      toolButton('✕', 'Delete', () => deleteThread(thread)),
    )
    li.append(tools)

    li.addEventListener('click', (e) => {
      if (e.target.closest('.tools') || e.target.closest('input')) return
      openThread(thread.id)
    })
    return li
  }

  function toolButton(glyph, label, onClick, pressed) {
    const b = document.createElement('button')
    b.type = 'button'
    b.textContent = glyph
    b.title = label
    b.setAttribute('aria-label', label)
    if (pressed !== undefined) b.setAttribute('aria-pressed', String(pressed))
    b.addEventListener('click', (e) => {
      e.stopPropagation()
      onClick()
    })
    return b
  }

  // A 404 from any thread route means the thread went — expired, or
  // deleted on another tab. Refresh the list and show whatever is current.
  async function vanished() {
    showNotice('That thread is gone. Showing the newest one.')
    await refreshThreads()
    const res = await fetch('/chat/history')
    if (res.ok) showTurns(await res.json())
  }

  async function openThread(id) {
    hideNotice()
    const res = await post(`/chat/threads/${id}/open`)
    if (res.status === 404) return vanished()
    if (!res.ok) {
      showNotice('Could not open that thread. Try again.')
      return
    }
    showTurns(await res.json())
    sideEl.classList.remove('open')
    menuButton.setAttribute('aria-expanded', 'false')
    await refreshThreads()
  }

  async function pinThread(thread) {
    const res = await post(`/chat/threads/${thread.id}/pin`, { pinned: !thread.pinned })
    if (res.status === 404) return vanished()
    if (!res.ok) showNotice('Could not change that. Try again.')
    await refreshThreads()
  }

  function renameInline(li, titleEl, thread) {
    const input = document.createElement('input')
    input.value = thread.title ?? ''
    input.maxLength = 80
    titleEl.replaceChildren(input)
    input.focus()
    input.select()
    let done = false
    const finish = async (save) => {
      if (done) return
      done = true
      const title = input.value.trim()
      if (save && title) {
        const res = await post(`/chat/threads/${thread.id}/rename`, { title })
        if (res.status === 404) return vanished()
        if (!res.ok) showNotice('Could not rename that thread. Try again.')
      }
      await refreshThreads()
    }
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') { e.preventDefault(); finish(true) }
      if (e.key === 'Escape') { e.preventDefault(); finish(false) }
    })
    input.addEventListener('blur', () => finish(true))
  }

  async function suggestTitle(thread) {
    hideNotice()
    const res = await post(`/chat/threads/${thread.id}/title`)
    if (res.status === 404) return vanished()
    if (!res.ok) {
      showNotice('Scout could not think of a name. Try again, or rename it yourself.')
      return
    }
    await refreshThreads()
  }

  async function deleteThread(thread) {
    const name = threadLabel(thread).text
    if (!window.confirm(`Delete "${name}"? This cannot be undone.`)) return
    const res = await post(`/chat/threads/${thread.id}/delete`)
    if (!res.ok && res.status !== 404) {
      showNotice('Could not delete that thread. Try again.')
      return
    }
    if (thread.id === currentThread) {
      await refreshThreads()
      const history = await fetch('/chat/history')
      showTurns(history.ok ? await history.json() : [])
    } else {
      await refreshThreads()
    }
  }

  menuButton.addEventListener('click', () => {
    const open = sideEl.classList.toggle('open')
    menuButton.setAttribute('aria-expanded', String(open))
  })

  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') refreshThreads()
  })
```

In `runMessage`, change the fetch body to `body: sendBody(text, currentThread),` and, right after `if (!res.ok || !res.body) {`, add first:

```js
      if (res.status === 404) {
        await vanished()
        return
      }
```

In the `finally` of `runMessage`, after `hideStatus()`, add `refreshThreads()` (not awaited) so the title set by the first answer appears.

In the `askForm` submit handler, before `running = true`, add:

```js
    if (currentThread === null) {
      const res = await post('/chat/threads')
      if (!res.ok) {
        showNotice('Could not start a thread. Reload to try again.')
        return
      }
      currentThread = (await res.json()).id
    }
```

Replace the `resetForm` submit handler body with:

```js
    e.preventDefault()
    hideNotice()
    const res = await post('/chat/threads')
    if (res.ok) {
      currentThread = (await res.json()).id
      turnsEl.replaceChildren()
      await refreshThreads()
    } else {
      showNotice('Could not start a new thread. Reload to try again.')
    }
```

Update the import list at the top of `start()`'s file is not needed — the pure functions are in the same module.

- [ ] **Step 2: Run every test**

Run: `cargo test -p scout-web && node --test 'crates/scout-web/src/*.test.mjs'`
Expected: PASS.

- [ ] **Step 3: Verify by hand**

Run the bot locally with a `.env` that has `SCOUT_SESSION_KEY`, `RESEND_API_KEY`, `SCOUT_MAIL_FROM`, `SCOUT_BASE_URL=http://localhost:8080` set (sign-in mounts only with all five). Open `/chat`, sign in, and check:
1. The sidebar lists threads; "New thread" adds one that becomes current and unnamed.
2. Sending a message names the thread after the answer.
3. Clicking another row switches, highlights it, and the composer sends into it.
4. Rename via ✎, Enter saves, Escape cancels. ✦ asks the model. 📌 pins and the row moves to the top. ✕ confirms then deletes.
5. Narrow the window under 720px: the ☰ button shows and toggles the drawer; opening a thread closes it.

- [ ] **Step 4: Commit**

```bash
git add crates/scout-web/src/chat.js
git commit -m "feat(web): switch, rename, pin and delete threads from the sidebar

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 14: README and the whole suite

**Files:**
- Modify: `README.md` — the "Remembers things" list and the `/chat` mention.

- [ ] **Step 1: Add the bullet**

Under **Remembers things**, after the "A conversation survives a restart" bullet, add:

```markdown
- **Threads, in the browser.** The web chat lists your conversations; switch
  between them and each keeps its own context, and the thread you last used
  is the one your Telegram chat continues. A thread nobody touches for two
  days is deleted — pin it and it stays until you delete it yourself
```

- [ ] **Step 2: Run everything**

```bash
cargo test --workspace && cargo clippy --workspace --all-targets && node --test 'crates/scout-web/src/*.test.mjs'
```

Expected: all pass, clippy clean apart from the pre-existing `proc-macro-error2` future-incompat note.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: threads in the browser

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

## Self-review against the spec

- Data: Task 1. Lifecycle (expiry, deletion, ownership, no cap): Tasks 2, 3, 8. Core API: Tasks 5, 6, 7. Routes incl. `send_message` thread id and `/chat/reset` alias: Task 10. Page: Tasks 11, 13. Mirror divider on switch: Tasks 9, 10. Errors (404 handling on the page, failed auto-rename notice): Task 13. Tests as listed in the spec: each task. Source assertions: Tasks 5, 8, 10.
- Names used across tasks: `threads_of`, `open_conversation`, `thread_title`, `set_thread_title`, `set_thread_title_if_missing`, `set_thread_pinned`, `delete_conversation`, `expire_conversations` (store); `threads`, `open_thread`, `rename`, `set_pinned`, `delete_thread`, `suggest_title`, `title_if_missing`, `first_message_title`, `clean_title` (session); `title_for` (agent); `note` (mirror); `Thread` (api). Checked consistent.
