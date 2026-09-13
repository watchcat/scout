# Debug Trace Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every run records a trace (each tool call with arguments, duration, status and result; nested flight-desk calls; run-level events). An admin types `/debug on` in the web chat and every Scout answer gets a Trace button that opens that run's trace, filled live while a run streams.

**Architecture:** A `runs` table and a `run_traces` table; `messages.run_id` links an answer to its run. An `Observer` (event sink + pulse + run id + trace rows) replaces the `events, pulse` pair handed to `build_agent`, the flight desk and the specialist, so top-level and nested calls are recorded by one piece of code. A new `AgentEvent::Trace` streams rows live. `scout_core::debug` is the door the web crate uses for the flag, admin check and trace reads. The client renders a panel per turn.

**Tech Stack:** Rust (axum 0.8, rig 0.40, DuckDB via `duckdb` crate, serde), vanilla JS module tested with `node --test`.

**Spec:** `docs/superpowers/specs/2026-09-13-debug-trace-design.md`. Two deviations from it, decided while planning: the run id reaches the browser as the first trace frame (`TraceFrame::Run`) rather than on the end frame, so `RunOutcome::Answered(String)` and the Telegram match sites stay untouched; and the `runs` table has no `channel` column, since `RunContext` does not know its channel and nothing reads it.

**Repo rules:** branch in the main checkout (not a worktree). Do NOT run `cargo fmt`. Rust tests: `cargo test -p <crate> <filter>`; JS tests: `node --test 'crates/scout-web/src/*.test.mjs'`. Watch each new test fail before making it pass. Commits end with `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`. Comments say why.

---

## File structure

| File | Responsibility |
|---|---|
| `crates/scout-api/src/lib.rs` | Wire types: `AgentEvent::Trace`, `TraceFrame`, `RunRow`, `TraceRow`, `Turn.run_id` |
| `crates/scout-telegram/src/progress.rs` | Ignores `Trace` |
| `crates/scout-core/src/store.rs` | Steps 12/13; `runs`/`run_traces` CRUD; `append_messages` with run id; `conversation_messages` with run id; `set_debug`/`debug_of`; `trim_traces` |
| `crates/scout-core/src/observer.rs` (new) | `Observer`: records rows, emits frames, owns the pulse |
| `crates/scout-core/src/run.rs` | Opens/closes the run, records events, handles `ToolResult`, saves traces |
| `crates/scout-core/src/agent.rs`, `flights.rs`, `specialist.rs` | Take `Arc<Observer>`; the specialist records nested rows |
| `crates/scout-core/src/session.rs` | Run ids on turns; `append_history` with run id |
| `crates/scout-core/src/core.rs` | `is_admin_account`; hourly `trim_traces` |
| `crates/scout-core/src/debug.rs` (new, pub) | `set`, `is_on`, `is_admin`, `trace`, `seed_run_for_tests` |
| `crates/scout-web/src/routes/chat.rs` | `/debug` command; `GET /chat/debug`; `GET /chat/runs/{id}/trace` |
| `crates/scout-web/src/chat.js`, `chat.html`, `chat.test.mjs` | Trace button, panel, live rows |
| `README.md`, `docs/BOARD.md` | Docs |

---

### Task 0: Branch

- [ ] `cd /Users/watchcat/work/rust/scout && git checkout main && git pull --ff-only && git checkout -b feat/debug-trace`

---

### Task 1: Wire types

**Files:** `crates/scout-api/src/lib.rs`, `crates/scout-telegram/src/progress.rs`

- [ ] **Step 1: Failing tests** in `crates/scout-api/src/lib.rs` `mod tests` (create the module at the end of the file if there is none):

```rust
    #[test]
    fn a_trace_frame_serialises_under_its_own_tag() {
        let f = AgentEvent::Trace(TraceFrame::Started {
            seq: 3, tool: "search_web".into(), args: serde_json::json!({"query": "beans"}), nested: false,
        });
        let json = serde_json::to_value(&f).unwrap();
        assert_eq!(json["Trace"]["kind"], "started");
        assert_eq!(json["Trace"]["seq"], 3);
        let back: AgentEvent = serde_json::from_value(json).unwrap();
        assert!(matches!(back, AgentEvent::Trace(TraceFrame::Started { seq: 3, .. })));
    }

    #[test]
    fn a_turn_without_a_run_still_parses() {
        // Turns saved before run ids existed, and every You turn.
        let t: Turn = serde_json::from_str(r#"{"role":"You","text":"hi"}"#).unwrap();
        assert_eq!(t.run_id, None);
    }
```

- [ ] **Step 2:** `cargo test -p scout-api` — compile error, `TraceFrame` unknown.

- [ ] **Step 3: Types.** In `crates/scout-api/src/lib.rs`, add to `AgentEvent`:

```rust
    /// One row of the run's trace, as it happens: a tool starting, a tool
    /// finishing, or something the run itself did. Recorded for every run
    /// and shown only to an admin with debug on; Telegram ignores it.
    Trace(TraceFrame),
```

and after `AgentEvent`:

```rust
/// A trace row on the wire. `seq` orders rows within a run and joins a
/// `Finished` to its `Started`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TraceFrame {
    /// Sent once, first, so the page can fetch the saved trace later.
    Run { run_id: i64 },
    Started { seq: i64, tool: String, args: serde_json::Value, nested: bool },
    Finished { seq: i64, duration_ms: i64, status: String, detail: Option<String> },
    Event { seq: i64, detail: String, error: bool },
}

/// A run as saved: what the trace panel heads with.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RunRow {
    pub id: i64,
    pub started_at: String,
    pub ended_at: Option<String>,
    /// `answered`, `cut_short` or `failed`; `None` while running.
    pub outcome: Option<String>,
    pub detail: Option<String>,
}

/// A trace row as saved. `kind` is `tool` or `event`. `result` is the
/// tool's result text, cut at the store's cap when `truncated`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TraceRow {
    pub seq: i64,
    pub kind: String,
    pub tool: Option<String>,
    pub args: Option<serde_json::Value>,
    pub nested: bool,
    pub started_at: String,
    pub duration_ms: Option<i64>,
    pub status: Option<String>,
    pub detail: Option<String>,
    pub result: Option<String>,
    pub truncated: bool,
}
```

Add to `Turn`:

```rust
    /// The run that produced a Scout turn, for its trace. `None` on You
    /// turns and on turns saved before runs were recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<i64>,
```

Every `Turn { role, text }` literal in the workspace gains `run_id: None` (grep `Turn {` in `scout-core/src/session.rs`, `scout-core/src/mirror.rs`, `scout-web/src/routes/chat.rs`).

In `crates/scout-telegram/src/progress.rs`, the `match event` gains:

```rust
            // Traces are for the browser's debug panel; a chat has nowhere
            // to put a table.
            AgentEvent::Trace(_) => {}
```

- [ ] **Step 4:** `cargo test --workspace` — green.

- [ ] **Step 5: Commit** `feat(api): trace frames and run ids on the wire`.

---

### Task 2: Store

**Files:** `crates/scout-core/src/store.rs`

- [ ] **Step 1: Failing tests** in `store.rs` `mod tests`:

```rust
    /// `accounts` and `messages` exactly as they stood at schema 11, before
    /// `debug` and `run_id`. Do NOT update when `MIGRATIONS` changes.
    const PRE_TRACE_TABLES: &str = r#"
CREATE SEQUENCE IF NOT EXISTS accounts_id_seq;
CREATE TABLE accounts (
    id         BIGINT PRIMARY KEY DEFAULT nextval('accounts_id_seq'),
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE SEQUENCE IF NOT EXISTS messages_id_seq;
CREATE TABLE messages (
    id              BIGINT PRIMARY KEY DEFAULT nextval('messages_id_seq'),
    conversation_id BIGINT NOT NULL,
    position        BIGINT NOT NULL,
    body            TEXT NOT NULL,
    created_at      TIMESTAMP NOT NULL DEFAULT current_timestamp
);
"#;

    #[test]
    fn a_version_11_database_gains_runs_traces_and_the_debug_flag() {
        // Both columns land on tables that already hold rows in production,
        // and `debug` must be NOT NULL at the end without DuckDB refusing
        // the constraint in the transaction that backfilled it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scout.duckdb");
        {
            let conn = duckdb::Connection::open(&path).unwrap();
            conn.execute_batch(PRE_TRACE_TABLES).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_version (version BIGINT NOT NULL);
                 INSERT INTO schema_version VALUES (11);
                 INSERT INTO accounts (id) VALUES (1);
                 INSERT INTO messages (conversation_id, position, body) VALUES (1, 0, '{}');",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), 13);
        assert!(!store.debug_of(1).unwrap(), "backfilled to off");
        let run_id: Option<i64> = store
            .conn()
            .query_row("SELECT run_id FROM messages WHERE position = 0", [], |r| r.get(0))
            .unwrap();
        assert_eq!(run_id, None);
        let nullable: bool = store
            .conn()
            .query_row("SELECT is_nullable = 'YES' FROM information_schema.columns WHERE table_name = 'accounts' AND column_name = 'debug'", [], |r| r.get(0))
            .unwrap();
        assert!(!nullable, "debug must end up NOT NULL");
    }

    fn a_row(seq: i64, tool: &str, result: Option<&str>) -> scout_api::TraceRow {
        scout_api::TraceRow {
            seq, kind: "tool".into(), tool: Some(tool.into()), args: Some(serde_json::json!({"q": seq})),
            nested: false, started_at: "2026-09-13T10:00:00Z".into(), duration_ms: Some(120),
            status: Some("ok".into()), detail: None, result: result.map(str::to_string), truncated: false,
        }
    }

    #[test]
    fn a_run_is_opened_traced_closed_and_read_back_by_its_owner_only() {
        let (_dir, store) = fresh();
        let me = store.account_for_telegram(1).unwrap();
        let other = store.account_for_telegram(2).unwrap();
        let conv = store.start_conversation(me, "direct").unwrap();

        let run_id = store.open_run(me, conv).unwrap();
        store.append_traces(run_id, &[a_row(0, "search_web", Some(r#"{"hits":3}"#)), a_row(1, "fetch_page", None)]).unwrap();
        store.close_run(run_id, "answered", None).unwrap();

        let (run, rows) = store.trace_of(run_id, me).unwrap().expect("the owner reads it");
        assert_eq!(run.outcome.as_deref(), Some("answered"));
        assert!(run.ended_at.is_some());
        assert_eq!(rows.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(rows[0].result.as_deref(), Some(r#"{"hits":3}"#));
        assert!(store.trace_of(run_id, other).unwrap().is_none(), "someone else's run is not found");
        assert!(store.trace_of(run_id + 100, me).unwrap().is_none());
    }

    #[test]
    fn a_result_past_the_cap_is_cut_and_says_so() {
        let (_dir, store) = fresh();
        let me = store.account_for_telegram(1).unwrap();
        let conv = store.start_conversation(me, "direct").unwrap();
        let run_id = store.open_run(me, conv).unwrap();
        let long = "é".repeat(TRACE_RESULT_CAP);  // 2 bytes each: over the cap, and a boundary to respect
        store.append_traces(run_id, &[a_row(0, "search_web", Some(&long))]).unwrap();
        let (_, rows) = store.trace_of(run_id, me).unwrap().unwrap();
        let kept = rows[0].result.as_deref().unwrap();
        assert!(rows[0].truncated);
        assert!(kept.len() <= TRACE_RESULT_CAP && kept.chars().all(|c| c == 'é'), "cut on a character boundary");
    }

    #[test]
    fn messages_remember_their_run_and_the_debug_flag_flips() {
        let (_dir, store) = fresh();
        let me = store.account_for_telegram(1).unwrap();
        let conv = store.start_conversation(me, "direct").unwrap();
        let run_id = store.open_run(me, conv).unwrap();
        store.append_messages(conv, None, &["{\"a\":1}".into()]).unwrap();
        store.append_messages(conv, Some(run_id), &["{\"b\":2}".into()]).unwrap();
        let got = store.conversation_messages(conv, 10).unwrap();
        assert_eq!(got, vec![(None, "{\"a\":1}".to_string()), (Some(run_id), "{\"b\":2}".to_string())]);

        assert!(!store.debug_of(me).unwrap());
        store.set_debug(me, true).unwrap();
        assert!(store.debug_of(me).unwrap());
        store.set_debug(me, false).unwrap();
        assert!(!store.debug_of(me).unwrap());
    }

    #[test]
    fn trimming_keeps_the_newest_runs_and_their_rows() {
        let (_dir, store) = fresh();
        let me = store.account_for_telegram(1).unwrap();
        let conv = store.start_conversation(me, "direct").unwrap();
        let ids: Vec<i64> = (0..5).map(|_| store.open_run(me, conv).unwrap()).collect();
        for id in &ids {
            store.append_traces(*id, &[a_row(0, "search_web", None)]).unwrap();
        }
        let gone = store.trim_traces(2).unwrap();
        assert_eq!(gone, 3);
        assert!(store.trace_of(ids[0], me).unwrap().is_none());
        assert!(store.trace_of(ids[4], me).unwrap().is_some());
        let rows: i64 = store.conn().query_row("SELECT count(*) FROM run_traces", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 2, "rows go with their runs");
    }
```

`fresh()` is whatever helper the file already uses to open a temp store (grep `fn fresh` or the pattern in `messages_are_appended` tests); use that name.

- [ ] **Step 2:** `cargo test -p scout-core store::a_run_is_opened` — compile errors.

- [ ] **Step 3: Migration.** In `MIGRATIONS` (the fresh-database block): add `debug BOOLEAN NOT NULL DEFAULT false` to `accounts`, `run_id BIGINT` to `messages`, and the two new tables after `messages`:

```sql
CREATE SEQUENCE IF NOT EXISTS runs_id_seq;
-- One row per agent run. The trace under a browser answer hangs off it,
-- and messages point at it so an answer can find its trace.
CREATE TABLE IF NOT EXISTS runs (
    id              BIGINT PRIMARY KEY DEFAULT nextval('runs_id_seq'),
    account_id      BIGINT NOT NULL,
    conversation_id BIGINT NOT NULL,
    started_at      TIMESTAMP NOT NULL DEFAULT current_timestamp,
    ended_at        TIMESTAMP,
    outcome         TEXT,
    detail          TEXT
);
CREATE SEQUENCE IF NOT EXISTS run_traces_id_seq;
CREATE TABLE IF NOT EXISTS run_traces (
    id          BIGINT PRIMARY KEY DEFAULT nextval('run_traces_id_seq'),
    run_id      BIGINT NOT NULL,
    seq         BIGINT NOT NULL,
    kind        TEXT NOT NULL,
    tool        TEXT,
    args        TEXT,
    nested      BOOLEAN NOT NULL DEFAULT false,
    started_at  TIMESTAMP NOT NULL,
    duration_ms BIGINT,
    status      TEXT,
    detail      TEXT,
    result      TEXT,
    truncated   BOOLEAN NOT NULL DEFAULT false
);
```

Steps, after `STEP_11_KEPT_TRIPS_NOT_NULL`:

```rust
/// Runs and their traces, the run id on messages, and the debug switch on
/// accounts. Same shape as steps 7/8 and 10/11, for the same DuckDB
/// reasons: `ADD COLUMN` cannot carry a constraint, and `SET NOT NULL`
/// refuses to share a transaction with the rows it just touched.
const STEP_12_RUN_TRACES: &str = r#"
CREATE SEQUENCE IF NOT EXISTS runs_id_seq;
CREATE TABLE IF NOT EXISTS runs (
    id BIGINT PRIMARY KEY DEFAULT nextval('runs_id_seq'),
    account_id BIGINT NOT NULL, conversation_id BIGINT NOT NULL,
    started_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    ended_at TIMESTAMP, outcome TEXT, detail TEXT
);
CREATE SEQUENCE IF NOT EXISTS run_traces_id_seq;
CREATE TABLE IF NOT EXISTS run_traces (
    id BIGINT PRIMARY KEY DEFAULT nextval('run_traces_id_seq'),
    run_id BIGINT NOT NULL, seq BIGINT NOT NULL, kind TEXT NOT NULL,
    tool TEXT, args TEXT, nested BOOLEAN NOT NULL DEFAULT false,
    started_at TIMESTAMP NOT NULL, duration_ms BIGINT, status TEXT,
    detail TEXT, result TEXT, truncated BOOLEAN NOT NULL DEFAULT false
);
ALTER TABLE messages ADD COLUMN IF NOT EXISTS run_id BIGINT;
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS debug BOOLEAN;
UPDATE accounts SET debug = false WHERE debug IS NULL;
ALTER TABLE accounts ALTER COLUMN debug SET DEFAULT false;
"#;

const STEP_13_DEBUG_NOT_NULL: &str = r#"
ALTER TABLE accounts ALTER COLUMN debug SET NOT NULL;
"#;
```

and in `steps()`: `(12, Step::Sql(STEP_12_RUN_TRACES)), (13, Step::Sql(STEP_13_DEBUG_NOT_NULL)),`. Update the existing `assert_eq!(s.schema_version().unwrap(), 11)` to 13.

- [ ] **Step 4: Methods.** Add to `impl Store`:

```rust
/// Bytes of a tool result kept on a trace row. A flight search is a few
/// thousand characters; a fetched page can be far more, and the page is
/// not what anyone reads a trace for.
pub const TRACE_RESULT_CAP: usize = 64 * 1024;

    pub fn open_run(&self, account_id: i64, conversation_id: i64) -> Result<i64> {
        let conn = self.conn();
        Ok(conn.query_row(
            "INSERT INTO runs (account_id, conversation_id) VALUES (?, ?) RETURNING id",
            params![account_id, conversation_id],
            |r| r.get(0),
        )?)
    }

    pub fn close_run(&self, run_id: i64, outcome: &str, detail: Option<&str>) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE runs SET ended_at = now(), outcome = ?, detail = ? WHERE id = ?",
            params![outcome, detail, run_id],
        )?;
        Ok(())
    }

    /// Writes a run's rows in one transaction, results cut at the cap.
    pub fn append_traces(&self, run_id: i64, rows: &[scout_api::TraceRow]) -> Result<()> {
        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<()> {
            for row in rows {
                let (result, truncated) = match &row.result {
                    Some(text) if text.len() > TRACE_RESULT_CAP => {
                        let mut cut = TRACE_RESULT_CAP;
                        while !text.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        (Some(&text[..cut]), true)
                    }
                    Some(text) => (Some(text.as_str()), row.truncated),
                    None => (None, false),
                };
                conn.execute(
                    "INSERT INTO run_traces (run_id, seq, kind, tool, args, nested, started_at, duration_ms, status, detail, result, truncated)
                     VALUES (?, ?, ?, ?, ?, ?, ?::TIMESTAMP, ?, ?, ?, ?, ?)",
                    params![
                        run_id, row.seq, row.kind, row.tool,
                        row.args.as_ref().map(|a| a.to_string()),
                        row.nested, row.started_at, row.duration_ms, row.status, row.detail,
                        result, truncated,
                    ],
                )?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
        conn.execute_batch("COMMIT")?;
        Ok(())
    }

    /// The run and its rows, or `None` when it is not this account's or
    /// no longer kept. Ownership is in the query so there is no way to read
    /// a trace without asking whose it is.
    pub fn trace_of(&self, run_id: i64, account_id: i64) -> Result<Option<(scout_api::RunRow, Vec<scout_api::TraceRow>)>> {
        let conn = self.conn();
        let run = conn
            .query_row(
                "SELECT id, started_at::TEXT, ended_at::TEXT, outcome, detail FROM runs WHERE id = ? AND account_id = ?",
                params![run_id, account_id],
                |r| Ok(scout_api::RunRow {
                    id: r.get(0)?, started_at: r.get(1)?, ended_at: r.get(2)?, outcome: r.get(3)?, detail: r.get(4)?,
                }),
            )
            .optional()?;
        let Some(run) = run else { return Ok(None) };
        let mut stmt = conn.prepare(
            "SELECT seq, kind, tool, args, nested, started_at::TEXT, duration_ms, status, detail, result, truncated
             FROM run_traces WHERE run_id = ? ORDER BY seq ASC",
        )?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                let args: Option<String> = r.get(3)?;
                Ok(scout_api::TraceRow {
                    seq: r.get(0)?, kind: r.get(1)?, tool: r.get(2)?,
                    args: args.and_then(|a| serde_json::from_str(&a).ok()),
                    nested: r.get(4)?, started_at: r.get(5)?, duration_ms: r.get(6)?,
                    status: r.get(7)?, detail: r.get(8)?, result: r.get(9)?, truncated: r.get(10)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Some((run, rows)))
    }

    /// Deletes everything but the newest `keep_runs` runs, rows included.
    /// Returns how many runs went.
    pub fn trim_traces(&self, keep_runs: usize) -> Result<usize> {
        let conn = self.conn();
        conn.execute(
            "DELETE FROM run_traces WHERE run_id NOT IN (SELECT id FROM runs ORDER BY id DESC LIMIT ?)",
            params![keep_runs as i64],
        )?;
        Ok(conn.execute(
            "DELETE FROM runs WHERE id NOT IN (SELECT id FROM runs ORDER BY id DESC LIMIT ?)",
            params![keep_runs as i64],
        )?)
    }

    pub fn set_debug(&self, account_id: i64, on: bool) -> Result<()> {
        let conn = self.conn();
        conn.execute("UPDATE accounts SET debug = ? WHERE id = ?", params![on, account_id])?;
        Ok(())
    }

    pub fn debug_of(&self, account_id: i64) -> Result<bool> {
        let conn = self.conn();
        Ok(conn
            .query_row("SELECT debug FROM accounts WHERE id = ?", params![account_id], |r| r.get(0))
            .optional()?
            .unwrap_or(false))
    }
```

`optional()` needs `use duckdb::OptionalExt;` if not already imported (grep). If `RETURNING` is refused by DuckDB in this version, insert then `SELECT currval('runs_id_seq')` inside the same lock.

`append_messages(&self, conversation_id: i64, run_id: Option<i64>, bodies: &[String])`: the INSERT becomes `INSERT INTO messages (id, conversation_id, position, body, run_id) VALUES (nextval('messages_id_seq'), ?, ?, ?, ?)` with `run_id` as the fourth param. Every caller in the crate passes `None` for now (grep `append_messages(`); `session::append_history` gets the real id in Task 5.

`conversation_messages` returns `Vec<(Option<i64>, String)>`: select `run_id, body`, map `(r.get(0)?, r.get(1)?)`. Update its callers (`session::load_history_raw` maps `.1` for now; Task 5 uses `.0`).

- [ ] **Step 5:** `cargo test -p scout-core store::` — green; `cargo build -p scout-core` — green.

- [ ] **Step 6: Commit** `feat(store): runs, their traces, the run id on messages and the debug switch`.

---

### Task 3: The `Observer`

**Files:** create `crates/scout-core/src/observer.rs`; `crates/scout-core/src/lib.rs` (`mod observer;`)

- [ ] **Step 1: Failing tests** — create the file with the tests only:

```rust
//! What a run hands to everything that works for it: where to report
//! progress, proof of life for the stall guard, and the trace.
//!
//! One struct rather than three parameters because the flight desk and its
//! specialist need all three, and a trace recorded by two different pieces
//! of code would disagree about what a failed tool looks like.

#[cfg(test)]
mod tests {
    use super::*;
    use scout_api::{AgentEvent, TraceFrame};
    use serde_json::json;

    fn observer() -> (Observer, tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) {
        let (events, seen) = tokio::sync::mpsc::unbounded_channel();
        (Observer::new(events, 42), seen)
    }

    fn frames(seen: &mut tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) -> Vec<TraceFrame> {
        let mut out = Vec::new();
        while let Ok(e) = seen.try_recv() {
            if let AgentEvent::Trace(f) = e { out.push(f) }
        }
        out
    }

    #[test]
    fn a_tool_that_starts_and_finishes_is_one_row_with_a_duration() {
        let (o, mut seen) = observer();
        o.tool_started("c1", "search_web", json!({"query": "beans"}), false);
        o.tool_finished("c1", r#"{"hits": 3}"#);
        let rows = o.take_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tool.as_deref(), Some("search_web"));
        assert_eq!(rows[0].status.as_deref(), Some("ok"));
        assert!(rows[0].duration_ms.is_some());
        assert_eq!(rows[0].result.as_deref(), Some(r#"{"hits": 3}"#));
        assert!(!rows[0].nested);
        let f = frames(&mut seen);
        assert!(matches!(f[0], TraceFrame::Started { seq: 0, ref tool, .. } if tool == "search_web"));
        assert!(matches!(f[1], TraceFrame::Finished { seq: 0, ref status, .. } if status == "ok"));
    }

    #[test]
    fn a_result_that_is_not_json_is_a_failed_tool() {
        // rig hands a tool's Err to the model as its text, and every Scout
        // tool returns a struct — so text that is not JSON is the error.
        let (o, mut seen) = observer();
        o.tool_started("c1", "search_flights", json!({}), true);
        o.tool_finished("c1", "duffel api error (status 429): slow down");
        let rows = o.take_rows();
        assert_eq!(rows[0].status.as_deref(), Some("failed"));
        assert_eq!(rows[0].detail.as_deref(), Some("duffel api error (status 429): slow down"));
        assert!(rows[0].nested);
        assert!(matches!(frames(&mut seen)[1], TraceFrame::Finished { ref detail, .. } if detail.is_some()));
    }

    #[test]
    fn a_finish_for_a_call_never_started_is_ignored() {
        let (o, _seen) = observer();
        o.tool_finished("ghost", "{}");
        assert!(o.take_rows().is_empty());
    }

    #[test]
    fn an_event_is_a_row_of_its_own() {
        let (o, mut seen) = observer();
        o.tool_started("c1", "search_web", json!({}), false);
        o.event("dead links in reply; asking the agent to correct it", false);
        o.event("the model call failed", true);
        let rows = o.take_rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].kind, "event");
        assert_eq!(rows[1].seq, 1);
        assert_eq!(rows[2].detail.as_deref(), Some("the model call failed"));
        assert_eq!(rows[2].status.as_deref(), Some("failed"), "an error event says so");
        let f = frames(&mut seen);
        assert!(matches!(f[2], TraceFrame::Event { error: true, .. }));
    }

    #[test]
    fn the_run_frame_goes_out_first() {
        let (o, mut seen) = observer();
        o.announce();
        assert!(matches!(frames(&mut seen)[0], TraceFrame::Run { run_id: 42 }));
    }
}
```

Add `mod observer;` to `lib.rs` next to `mod specialist;`.

- [ ] **Step 2:** `cargo test -p scout-core observer::` — compile error.

- [ ] **Step 3: Implement** above the tests:

```rust
use scout_api::{AgentEvent, EventSink, TraceFrame, TraceRow};
use std::collections::HashMap;
use std::sync::Mutex;

pub struct Observer {
    pub events: EventSink,
    pub pulse: crate::run::Pulse,
    pub run_id: i64,
    rows: Mutex<Vec<TraceRow>>,
    /// Calls started and not yet finished, by rig's internal call id, to
    /// the row they opened and the instant they started.
    open: Mutex<HashMap<String, (usize, std::time::Instant)>>,
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl Observer {
    pub fn new(events: EventSink, run_id: i64) -> Self {
        Self {
            events,
            pulse: crate::run::Pulse::default(),
            run_id,
            rows: Mutex::new(Vec::new()),
            open: Mutex::new(HashMap::new()),
        }
    }

    fn rows(&self) -> std::sync::MutexGuard<'_, Vec<TraceRow>> {
        self.rows.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self, frame: TraceFrame) {
        scout_api::emit(&self.events, AgentEvent::Trace(frame));
    }

    /// Sent once, first, so the page knows which run a live panel belongs
    /// to before any row arrives.
    pub fn announce(&self) {
        self.emit(TraceFrame::Run { run_id: self.run_id });
    }

    pub fn tool_started(&self, call_id: &str, tool: &str, args: serde_json::Value, nested: bool) {
        let seq = {
            let mut rows = self.rows();
            let seq = rows.len() as i64;
            rows.push(TraceRow {
                seq, kind: "tool".into(), tool: Some(tool.to_string()), args: Some(args.clone()),
                nested, started_at: now_iso(), duration_ms: None, status: None, detail: None,
                result: None, truncated: false,
            });
            seq
        };
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(call_id.to_string(), (seq as usize, std::time::Instant::now()));
        self.emit(TraceFrame::Started { seq, tool: tool.to_string(), args, nested });
    }

    /// The tool answered. rig hands a tool's `Err` to the model as the
    /// error's text, and every Scout tool returns a serialised struct, so a
    /// result that is not JSON is a tool that failed — the rule the flight
    /// desk's collector already applies.
    pub fn tool_finished(&self, call_id: &str, text: &str) {
        let Some((index, started)) = self.open.lock().unwrap_or_else(|e| e.into_inner()).remove(call_id) else {
            return;
        };
        let failed = serde_json::from_str::<serde_json::Value>(text).is_err();
        let duration_ms = started.elapsed().as_millis() as i64;
        let status = if failed { "failed" } else { "ok" }.to_string();
        let detail = failed.then(|| text.to_string());
        let seq = {
            let mut rows = self.rows();
            let row = &mut rows[index];
            row.duration_ms = Some(duration_ms);
            row.status = Some(status.clone());
            row.detail = detail.clone();
            row.result = Some(text.to_string());
            row.seq
        };
        self.emit(TraceFrame::Finished { seq, duration_ms, status, detail });
    }

    /// Something the run did on its own: a repair, a salvage, a failure.
    /// Worded exactly as the log line beside it.
    pub fn event(&self, detail: &str, error: bool) {
        let seq = {
            let mut rows = self.rows();
            let seq = rows.len() as i64;
            rows.push(TraceRow {
                seq, kind: "event".into(), tool: None, args: None, nested: false,
                started_at: now_iso(), duration_ms: None,
                status: Some(if error { "failed" } else { "ok" }.to_string()),
                detail: Some(detail.to_string()), result: None, truncated: false,
            });
            seq
        };
        self.emit(TraceFrame::Event { seq, detail: detail.to_string(), error });
    }

    /// Everything recorded so far, for saving. Leaves the observer empty.
    pub fn take_rows(&self) -> Vec<TraceRow> {
        std::mem::take(&mut *self.rows())
    }
}
```

`chrono` is already a dependency. `TraceRow` needs `Clone` (it has it from Task 1).

- [ ] **Step 4:** `cargo test -p scout-core observer::` — 5 pass.

- [ ] **Step 5: Commit** `feat(observer): one recorder for progress, liveness and the trace`.

---

### Task 4: Plumb the observer through the run, the agent and the flight desk

**Files:** `crates/scout-core/src/run.rs`, `agent.rs`, `flights.rs`, `specialist.rs`

- [ ] **Step 1: Failing tests.**

In `run.rs` `mod tests`, replace `the_stall_guard_reads_the_pulse_not_the_stream_alone`'s second assertion target with `observer.pulse.since() < STREAM_STALL` and replace `the_run_loop_hands_the_sink_to_the_agent_build` (in `agent.rs` tests) with:

```rust
    #[test]
    fn the_run_loop_hands_one_observer_to_the_agent_build() {
        let src = include_str!("run.rs");
        let src = &src[..src.find("#[cfg(test)]").expect("the tests must come last")];
        assert!(src.contains("build_agent(&core.deps, run, &facts, observer.clone())"), "the observer must reach build_agent");
    }
```

Add to `run.rs` tests:

```rust
    #[test]
    fn every_warning_about_the_run_is_also_a_trace_event() {
        // The log on the node and the trace under the answer must tell the
        // same story; a warning with no event is a failure the admin
        // cannot see from the browser.
        let src = include_str!("run.rs");
        let body = &src[..src.find("#[cfg(test)]").expect("the tests must come last")];
        for wording in [
            "run interrupted; writing up from notes",
            "the model wrote a tool call as text; asking it to answer",
            "dead links in reply; asking the agent to correct it",
            "dead links survived the correction; stripping",
        ] {
            let at = body.find(wording).unwrap_or_else(|| panic!("{wording:?} is no longer logged"));
            let after = &body[at..];
            assert!(
                after[..after.find('\n').unwrap_or(after.len()) + 400].contains("observer.event("),
                "{wording:?} is logged but not traced"
            );
        }
        assert!(body.contains(r#"finish_run(&core.deps.store, &observer, "failed""#), "a failed run closes its row");
    }

    #[test]
    fn the_run_is_opened_before_the_agent_and_traces_are_saved_after_the_messages() {
        let src = include_str!("run.rs");
        let body = &src[..src.find("#[cfg(test)]").expect("the tests must come last")];
        let open = body.find("open_run(").expect("the run must be opened");
        let build = body.find("build_agent(").expect("the agent build must exist");
        let append = body.find("append_history(").expect("messages are appended");
        let save = body.rfind("finish_run(").expect("traces are saved");
        assert!(open < build, "the run id must exist before the observer is built");
        assert!(append < save, "traces are saved after the messages that point at the run");
    }
```

In `specialist.rs` tests, the helper `specialist_with_pulse(agent, events, pulse)` becomes `specialist_with(agent, observer: Arc<Observer>)`; the test that checks the pulse builds `Observer::new(events, 1)` and swaps its `pulse` for `Pulse::aged(..)` (make `pulse` a plain pub field, so `let mut o = Observer::new(..); o.pulse = Pulse::aged(..);` works before wrapping in `Arc`). Add:

```rust
    #[tokio::test]
    async fn nested_calls_are_recorded_on_the_observer_as_nested() {
        let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
        let observer = std::sync::Arc::new(crate::observer::Observer::new(events, 7));
        let tool = specialist_with(scripted(7, 5), observer.clone());
        rig::tool::Tool::call(&tool, Brief { brief: "probe".to_string() }).await.unwrap();
        let rows = observer.take_rows();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].nested);
        assert_eq!(rows[0].tool.as_deref(), Some("probe"));
        assert_eq!(rows[0].status.as_deref(), Some("ok"));
    }
```

In `flights.rs` the `ask_flights` test builds `Arc::new(Observer::new(events, 1))` and passes it instead of `events, pulse`.

- [ ] **Step 2:** `cargo test -p scout-core` — compile errors.

- [ ] **Step 3: Signatures.**
  - `agent.rs`: `build_agent(d, run, facts, observer: Arc<crate::observer::Observer>)`; the `ask_flights` call passes `observer`. The `Arc<Pulse>`/`EventSink` params go.
  - `flights.rs`: `ask_flights(d, run, facts, budget, observer: Arc<Observer>)`; `Specialist { .., observer, .. }`.
  - `specialist.rs`: replace fields `events` and `pulse` with `pub observer: Arc<crate::observer::Observer>`. In the loop: `self.observer.pulse.touch()` where `self.pulse.touch()` was; the collector's `tool_started` gets `&self.observer.events` (it takes a sink today); beside `collector.tool_started(..)` add `self.observer.tool_started(&internal_call_id, &tool_call.function.name, tool_call.function.arguments.clone(), true);` and beside `collector.tool_finished(..)` add `self.observer.tool_finished(&internal_call_id, &text);`.

- [ ] **Step 4: `run.rs`.** After `take_slot` succeeds and before `build_agent`:

```rust
    let run_id = {
        let store = core.deps.store.clone();
        crate::core::blocking(move || store.open_run(account_id, conversation_id)).await?
    };
    let observer = std::sync::Arc::new(crate::observer::Observer::new(events.clone(), run_id));
    observer.announce();
    let agent = build_agent(&core.deps, run, &facts, observer.clone());
```

The stall guard uses `observer.pulse` (delete the `let pulse = ...` line). In the loop, the `ToolExecutionStart` arm also records: `observer.tool_started(&internal_call_id, &tool_call.function.name, args.clone(), false);` (bind `internal_call_id` in the pattern). Add an arm before `_ => {}`:

```rust
                MultiTurnStreamItem::StreamUserItem(rig::streaming::StreamedUserContent::ToolResult {
                    tool_result,
                    internal_call_id,
                }) => {
                    let text = tool_result
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            rig::completion::message::ToolResultContent::Text(t) => Some(t.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    observer.tool_finished(&internal_call_id, &text);
                }
```

Add a helper below `run_agent`:

```rust
/// Writes the trace and closes the run, on every exit that has one. Never
/// fails the reply over it: the answer is already on its way.
async fn finish_run(store: &crate::store::Store, observer: &crate::observer::Observer, outcome: &str, detail: Option<&str>) {
    let rows = observer.take_rows();
    let (store, run_id, outcome, detail) = (store.clone(), observer.run_id, outcome.to_string(), detail.map(str::to_string));
    if let Err(e) = crate::core::blocking(move || {
        store.append_traces(run_id, &rows)?;
        store.close_run(run_id, &outcome, detail.as_deref())
    })
    .await
    {
        tracing::warn!(error = %e, run_id, "could not save the trace");
    }
}
```

Wire it:
  - `Ok(Err(e))` branch (the run failed): before `return Err(e)`, `observer.event("the model call failed", true); finish_run(&core.deps.store, &observer, "failed", Some(&e.to_string())).await;`.
  - The `bail!` sites ("no answer and no notes", "wrap-up came back with no answer", "wrote a tool call as text, twice", "wrap-up timed out"): each becomes `observer.event(<the bail message>, true); finish_run(.., "failed", Some(<message>)).await; anyhow::bail!(..)`. Keep it readable with a small closure or by computing the message once.
  - Beside each `tracing::warn!` named in the test, `observer.event("<same wording>", <error>)` with `error = true` only for "dead links survived the correction; stripping".
  - After `append_history` (Task 5 gives it the run id), before `Ok(RunOutcome::Answered(reply))`: `finish_run(&core.deps.store, &observer, if salvaged { "cut_short" } else { "answered" }, salvage_reason).await;` where `salvaged`/`salvage_reason` are what `salvage` held before the wrap-up consumed it (keep a copy: `let cut_short: Option<&'static str> = salvage;` right after it is final).

Until Task 5, call `append_history(&store, conversation_id, None, &added)`? No: `append_history` still has its old signature until Task 5; leave that call unchanged in this task.

- [ ] **Step 5:** `cargo test -p scout-core` — green (the source assertions on `append_history` ordering pass already; the run id is added in Task 5). `cargo test --workspace` — green.

- [ ] **Step 6: Commit** `feat(run): every run is opened, traced and closed`.

---

### Task 5: Session, admin check and maintenance

**Files:** `crates/scout-core/src/session.rs`, `core.rs`, `run.rs`

- [ ] **Step 1: Failing tests.**

`session.rs` tests:

```rust
    #[test]
    fn a_scout_turn_carries_its_run_and_a_you_turn_does_not() {
        let history = vec![user_text("beans?"), assistant_text("three brands")];
        let runs = vec![None, Some(9)];
        let turns = turns_of(&history, &runs);
        assert_eq!(turns[0].run_id, None);
        assert_eq!(turns[1].run_id, Some(9));
        // A message saved before runs existed.
        let turns = turns_of(&history, &[None, None]);
        assert_eq!(turns[1].run_id, None);
    }
```

(`user_text`/`assistant_text` are the helpers the file's tests already use for building messages; use those names.) Every existing `turns_of(&history)` call in tests becomes `turns_of(&history, &vec![None; history.len()])`.

`core.rs` tests:

```rust
    #[tokio::test]
    async fn an_account_is_an_admin_when_one_of_its_telegram_ids_is() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.duckdb").to_str().unwrap().to_string();
        // `Config::for_test`: 111 is allowed and, being first, the admin.
        let core = Core::start(crate::config::Config::for_test(&p), None).unwrap();
        let store = core.store();
        let admin = store.account_for_telegram(111).unwrap();
        let member = store.account_for_telegram(222).unwrap();
        assert!(core.is_admin_account(admin).await.unwrap());
        assert!(!core.is_admin_account(member).await.unwrap());
    }

    #[test]
    fn maintenance_trims_traces_beside_the_message_logs() {
        let src = include_str!("core.rs");
        assert!(src.contains("trim_traces(Self::TRACE_RUNS_KEEP)"), "traces must be trimmed in maintenance");
    }
```

- [ ] **Step 2:** `cargo test -p scout-core session::a_scout_turn core::an_account` — compile errors.

- [ ] **Step 3: Implement.**

`session.rs`:
  - `load_history_raw` keeps its signature and maps `.1`; add
    ```rust
    /// The transcript with the run each message came from, for the page.
    pub(crate) fn load_history_with_runs(store: &Store, conversation_id: i64, cap: usize) -> anyhow::Result<(Vec<LlmMessage>, Vec<Option<i64>>)> {
        let mut messages = Vec::new();
        let mut runs = Vec::new();
        for (run_id, body) in store.conversation_messages(conversation_id, cap)? {
            match serde_json::from_str::<LlmMessage>(&body) {
                Ok(m) => { messages.push(m); runs.push(run_id); }
                Err(e) => tracing::warn!(error = %e, "dropping an unreadable stored message"),
            }
        }
        Ok((messages, runs))
    }
    ```
  - `turns_of(history: &[LlmMessage], runs: &[Option<i64>])`: the Scout arm sets `run_id: runs.get(i).copied().flatten()`, the You arm `run_id: None`.
  - `transcript_of` uses `load_history_with_runs` and passes both.
  - `append_history(store, conversation_id, run_id: Option<i64>, messages)` → `store.append_messages(conversation_id, run_id, &bodies_of(messages)?)`. Update its callers: `run.rs` passes `Some(run_id)`; `seed_exchange_for_tests` and any other pass `None`.

`core.rs`:
  ```rust
    /// Whether any Telegram identity on this account is an admin. The web
    /// has accounts, not Telegram ids, and this is the one place the two
    /// are joined for the purpose.
    pub async fn is_admin_account(&self, account_id: i64) -> anyhow::Result<bool> {
        let store = self.store();
        let ids = blocking(move || store.telegram_ids(account_id)).await?;
        Ok(ids.into_iter().any(|id| self.is_admin(crate::ids::TelegramId(id))))
    }
  ```
  and `const TRACE_RUNS_KEEP: usize = 300;` with, in `run_maintenance` beside the `trim_message_logs` match, the same shape calling `self.trim_traces()`, a private async fn that does `blocking(move || store.trim_traces(Self::TRACE_RUNS_KEEP))`.

- [ ] **Step 4:** `cargo test -p scout-core` — green. The Task 4 source assertion about `append_history` still holds.

- [ ] **Step 5: Commit** `feat(session): a Scout turn names its run; admins are accounts too; traces are trimmed`.

---

### Task 6: The `debug` door and the web routes

**Files:** create `crates/scout-core/src/debug.rs`; `crates/scout-core/src/lib.rs` (`pub mod debug;`); `crates/scout-web/src/routes/chat.rs`

- [ ] **Step 1: `debug.rs`** (small enough that its tests are the web tests below plus one unit test):

```rust
//! The debug switch and the trace behind an answer, as the web sees them.
//! The only door through `Store` for either, kept narrow on purpose.

use crate::core::{blocking, Core};

pub async fn set(core: &Core, account_id: i64, on: bool) -> anyhow::Result<()> {
    let store = core.store();
    blocking(move || store.set_debug(account_id, on)).await
}

pub async fn is_on(core: &Core, account_id: i64) -> anyhow::Result<bool> {
    let store = core.store();
    blocking(move || store.debug_of(account_id)).await
}

/// The run and its rows, or `None` when it is not this account's or no
/// longer kept. Does not check the flag: the route does, so the reason can
/// be told apart.
pub async fn trace(core: &Core, run_id: i64, account_id: i64) -> anyhow::Result<Option<(scout_api::RunRow, Vec<scout_api::TraceRow>)>> {
    let store = core.store();
    blocking(move || store.trace_of(run_id, account_id)).await
}

/// A finished run with rows, for the web crate's tests, which cannot reach
/// `Store`. Returns the run id.
#[doc(hidden)]
pub async fn seed_run_for_tests(core: &Core, account_id: i64, rows: Vec<scout_api::TraceRow>) -> anyhow::Result<i64> {
    let store = core.store();
    blocking(move || {
        let conv = store.start_conversation(account_id, "direct")?;
        let id = store.open_run(account_id, conv)?;
        store.append_traces(id, &rows)?;
        store.close_run(id, "answered", None)?;
        Ok(id)
    })
    .await
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn the_switch_is_per_account_and_off_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.duckdb").to_str().unwrap().to_string();
        let core = crate::core::Core::start(crate::config::Config::for_test(&p), None).unwrap();
        let a = core.store().account_for_telegram(1).unwrap();
        assert!(!super::is_on(&core, a).await.unwrap());
        super::set(&core, a, true).await.unwrap();
        assert!(super::is_on(&core, a).await.unwrap());
    }
}
```

Add `pub mod debug;` to `lib.rs`.

- [ ] **Step 2: Failing web tests** in `crates/scout-web/src/routes/chat.rs` `mod tests`:

```rust
    /// Sends one chat message as `session` and returns the whole SSE body.
    async fn chat_body(app: &axum::Router, session: &str, csrf: &str, thread: i64, text: &str) -> String {
        let res = post_json_with_cookie(app, "/chat/messages", session, Some(csrf),
            &format!(r#"{{"text":{},"thread":{thread}}}"#, serde_json::to_string(text).unwrap())).await;
        assert_eq!(res.status(), StatusCode::OK);
        crate::tests::body_of(res).await
    }

    #[tokio::test]
    async fn debug_is_an_admin_switch_that_spends_nothing() {
        // `Config::for_test` makes telegram 111 the admin; 777 is a member.
        let (app, core, _dir) = test_app_with_a_round().await;
        let member = admitted(&core, "777").await;
        let (session, csrf) = signed_in(&core, member);
        let thread = new_thread_for(&core, member).await;

        let body = chat_body(&app, &session, &csrf, thread, "/debug on").await;
        assert!(body.contains("admin switch"), "{body}");
        assert!(!scout_core::debug::is_on(&core, member).await.unwrap());
        assert_eq!(core.requests_today(member).await.unwrap(), 0, "a refused switch is not a request");

        let admin = admitted(&core, "111").await;
        let (session, csrf) = signed_in(&core, admin);
        let thread = new_thread_for(&core, admin).await;
        let body = chat_body(&app, &session, &csrf, thread, "/debug on").await;
        assert!(body.contains("Debug is on"), "{body}");
        assert!(scout_core::debug::is_on(&core, admin).await.unwrap());
        assert_eq!(core.requests_today(admin).await.unwrap(), 0, "a switch is not a request");

        let res = get_with_cookie(&app, "/chat/debug", &session).await;
        assert_eq!(crate::tests::body_of(res).await, r#"{"on":true}"#);

        let body = chat_body(&app, &session, &csrf, thread, "/debug").await;
        assert!(body.contains("Debug is on"), "bare /debug reports: {body}");
        let body = chat_body(&app, &session, &csrf, thread, "/debug off").await;
        assert!(body.contains("Debug is off"), "{body}");
        assert!(!scout_core::debug::is_on(&core, admin).await.unwrap());
        // Nothing entered the conversation.
        let res = get_with_cookie(&app, "/chat/history", &session).await;
        assert_eq!(crate::tests::body_of(res).await, "[]");
    }

    #[tokio::test]
    async fn a_trace_is_read_by_its_owner_with_debug_on_and_by_nobody_else() {
        let (app, core, _dir) = test_app_with_a_round().await;
        let owner = admitted(&core, "111").await;
        let stranger = admitted(&core, "777").await;
        let row = scout_api::TraceRow {
            seq: 0, kind: "tool".into(), tool: Some("search_web".into()), args: Some(serde_json::json!({"query": "beans"})),
            nested: false, started_at: "2026-09-13T10:00:00Z".into(), duration_ms: Some(80),
            status: Some("ok".into()), detail: None, result: Some(r#"{"hits":1}"#.into()), truncated: false,
        };
        let run_id = scout_core::debug::seed_run_for_tests(&core, owner, vec![row]).await.unwrap();

        let (session, _) = signed_in(&core, owner);
        let res = get_with_cookie(&app, &format!("/chat/runs/{run_id}/trace"), &session).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "debug is off");

        scout_core::debug::set(&core, owner, true).await.unwrap();
        let res = get_with_cookie(&app, &format!("/chat/runs/{run_id}/trace"), &session).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&crate::tests::body_of(res).await).unwrap();
        assert_eq!(body["run"]["outcome"], "answered");
        assert_eq!(body["rows"][0]["tool"], "search_web");
        assert_eq!(body["rows"][0]["result"], r#"{"hits":1}"#);

        let (session, _) = signed_in(&core, stranger);
        scout_core::debug::set(&core, stranger, true).await.unwrap();
        let res = get_with_cookie(&app, &format!("/chat/runs/{run_id}/trace"), &session).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "someone else's run does not exist for them");
        let res = get_with_cookie(&app, "/chat/runs/999999/trace", &session).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
```

`signed_in(core, account_id) -> (session cookie value, csrf token)` and `new_thread_for(core, account_id) -> i64`: the file's existing thread tests already do both (grep `session::mint(` and `csrf_for(` and `start_thread`/`new_thread`); wrap whatever they do in these two helpers rather than duplicating. `get_with_cookie` is `crate::tests::get_with_cookie`. `core.requests_today` exists (used by `/stat`); if its name differs, use the function `/stat` uses.

- [ ] **Step 3:** `cargo test -p scout-web debug_is_an_admin` — fails (body has no "admin switch"; routes missing).

- [ ] **Step 4: Routes.** In `routes()` add `.route("/chat/debug", get(debug_state))` and `.route("/chat/runs/{id}/trace", get(run_trace))`. In `send_message`, right after the CSRF check and before the daily-cap check:

```rust
    // The debug switch is a command to the server, not a message to the
    // model: nothing is spent, logged or stored, and the reply is one
    // sentence in an otherwise empty stream. Admins only, because a trace
    // carries tool arguments and provider error text.
    if let Some(word) = debug_command(&body.text) {
        let sentence = match auth.core.is_admin_account(account_id).await {
            Ok(false) => "That's an admin switch.".to_string(),
            Ok(true) => match word {
                Some(on) => match scout_core::debug::set(&auth.core, account_id, on).await {
                    Ok(()) if on => "Debug is on. Each answer now shows its trace.".to_string(),
                    Ok(()) => "Debug is off.".to_string(),
                    Err(e) => {
                        tracing::error!(error = %e, "could not set the debug switch");
                        return sorry();
                    }
                },
                None => match scout_core::debug::is_on(&auth.core, account_id).await {
                    Ok(true) => "Debug is on.".to_string(),
                    Ok(false) => "Debug is off.".to_string(),
                    Err(e) => {
                        tracing::error!(error = %e, "could not read the debug switch");
                        return sorry();
                    }
                },
            },
            Err(e) => {
                tracing::error!(error = %e, "could not tell whether an account is an admin");
                return sorry();
            }
        };
        let (frames, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = frames.send(Frame::End(End::Ok { answer: sentence }));
        return sse_response(rx);
    }
```

with, outside the handler:

```rust
/// `Some(Some(true))` for `/debug on`, `Some(Some(false))` for `/debug
/// off`, `Some(None)` for bare `/debug`, `None` for anything else — a
/// message that merely starts with the word is a message.
fn debug_command(text: &str) -> Option<Option<bool>> {
    let mut words = text.trim().split_whitespace();
    if words.next()? != "/debug" {
        return None;
    }
    match (words.next(), words.next()) {
        (None, _) => Some(None),
        (Some("on"), None) => Some(Some(true)),
        (Some("off"), None) => Some(Some(false)),
        _ => None,
    }
}
```

and a unit test: `/debug on`, `/debug  off `, `/debug`, and `None` for `/debugger`, `/debug on please`, `debug on`.

The two handlers:

```rust
async fn debug_state(axum::extract::State(auth): axum::extract::State<AuthState>, headers: HeaderMap) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    match scout_core::debug::is_on(&auth.core, account_id).await {
        Ok(on) => axum::Json(serde_json::json!({"on": on})).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "could not read the debug switch");
            sorry()
        }
    }
}

/// The trace behind one answer. 403 with debug off so the page can say
/// "turn debug on"; 404 for a run that is not this account's or is no
/// longer kept, which look the same on purpose.
async fn run_trace(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
    axum::extract::Path(run_id): axum::extract::Path<i64>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    match scout_core::debug::is_on(&auth.core, account_id).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::FORBIDDEN.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "could not read the debug switch");
            return sorry();
        }
    }
    match scout_core::debug::trace(&auth.core, run_id, account_id).await {
        Ok(Some((run, rows))) => axum::Json(serde_json::json!({"run": run, "rows": rows})).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "could not read a trace");
            sorry()
        }
    }
}
```

Add both to the no-store/CSRF source tests if the file has a list of routes that must be listed (grep the test that enumerates `.route(` lines, e.g. `someone_elses_thread_is_not_found_on_every_route`; the trace route is a GET and needs no CSRF).

- [ ] **Step 5:** `cargo test -p scout-web` and `cargo test -p scout-core debug::` — green.

- [ ] **Step 6: Commit** `feat(web): /debug on and off, and the trace behind an answer`.

---

### Task 7: The page

**Files:** `crates/scout-web/src/chat.js`, `chat.html`, `chat.test.mjs`

- [ ] **Step 1: Failing JS tests** in `chat.test.mjs` (add `traceLines`, `applyTraceFrame`, `traceDuration` to the import list):

```js
test('a saved trace becomes one line per row, nested rows marked, events full width', () => {
  const run = { id: 5, outcome: 'cut_short', detail: 'it took too long', started_at: '2026-09-13T10:00:00Z', ended_at: '2026-09-13T10:01:30Z' }
  const rows = [
    { seq: 0, kind: 'tool', tool: 'ask_flights', args: { brief: 'AMS-LIS' }, nested: false, duration_ms: 42000, status: 'ok', result: '{"summary":"x"}', truncated: false },
    { seq: 1, kind: 'tool', tool: 'search_flights', args: { origin: 'AMS' }, nested: true, duration_ms: 8000, status: 'failed', detail: 'duffel 429', result: 'duffel 429', truncated: false },
    { seq: 2, kind: 'event', detail: 'run interrupted; writing up from notes', status: 'ok' },
  ]
  const lines = traceLines(run, rows)
  assert.equal(lines.head, 'cut_short · 1m 30s · it took too long')
  assert.deepEqual(lines.rows.map(r => [r.kind, r.nested, r.status, r.label]), [
    ['tool', false, 'ok', 'ask_flights {"brief":"AMS-LIS"}'],
    ['tool', true, 'failed', 'search_flights {"origin":"AMS"}'],
    ['event', false, 'ok', 'run interrupted; writing up from notes'],
  ])
  assert.equal(lines.rows[0].duration, '42.0s')
  assert.equal(lines.rows[1].detail, 'duffel 429')
})

test('arguments on a line are cut at 120 characters', () => {
  const rows = [{ seq: 0, kind: 'tool', tool: 't', args: { q: 'x'.repeat(200) }, nested: false, status: 'ok' }]
  const line = traceLines({ id: 1 }, rows).rows[0].label
  // 't', a space, 120 characters, the ellipsis.
  assert.ok(line.length <= 123 && line.endsWith('…'), line)
})

test('live frames build the same rows a saved trace would', () => {
  let rows = []
  rows = applyTraceFrame(rows, { kind: 'started', seq: 0, tool: 'search_web', args: { query: 'beans' }, nested: false })
  assert.equal(rows[0].status, undefined, 'still running')
  rows = applyTraceFrame(rows, { kind: 'finished', seq: 0, duration_ms: 300, status: 'ok', detail: null })
  rows = applyTraceFrame(rows, { kind: 'event', seq: 1, detail: 'dead links survived the correction; stripping', error: true })
  const lines = traceLines(null, rows)
  assert.deepEqual(lines.rows.map(r => [r.kind, r.status, r.duration]), [['tool', 'ok', '0.3s'], ['event', 'failed', '']])
  // A finish for a row that never started is ignored, as on the server.
  assert.equal(applyTraceFrame(rows, { kind: 'finished', seq: 9, duration_ms: 1, status: 'ok' }).length, 2)
})

test('durations read as seconds under a minute and minutes above', () => {
  assert.equal(traceDuration(900), '0.9s')
  assert.equal(traceDuration(42000), '42.0s')
  assert.equal(traceDuration(90000), '1m 30s')
  assert.equal(traceDuration(undefined), '')
})
```

- [ ] **Step 2:** `node --test 'crates/scout-web/src/*.test.mjs'` — fails on the import.

- [ ] **Step 3: Pure functions** in `chat.js`, next to the other exports:

```js
// The trace panel's model. Pure so it can be tested without a DOM, and
// shared by the saved trace (rows from the server) and the live one (rows
// built from frames), which must read identically.
const ARGS_LINE_CAP = 120

export function traceDuration(ms) {
  if (typeof ms !== 'number') return ''
  if (ms < 60000) return `${(ms / 1000).toFixed(1)}s`
  const m = Math.floor(ms / 60000)
  const s = Math.round((ms % 60000) / 1000)
  return `${m}m ${String(s).padStart(2, '0')}s`
}

function argsLine(args) {
  const text = args === undefined || args === null ? '' : JSON.stringify(args)
  return text.length > ARGS_LINE_CAP ? text.slice(0, ARGS_LINE_CAP) + '…' : text
}

export function traceLines(run, rows) {
  const head = run && run.outcome
    ? [run.outcome, run.ended_at && run.started_at ? traceDuration(Date.parse(run.ended_at) - Date.parse(run.started_at)) : '', run.detail]
        .filter(Boolean).join(' · ')
    : ''
  return {
    head,
    rows: rows.map(r => ({
      seq: r.seq,
      kind: r.kind,
      nested: Boolean(r.nested),
      status: r.status,
      label: r.kind === 'tool' ? `${r.tool} ${argsLine(r.args)}`.trimEnd() : (r.detail || ''),
      duration: r.kind === 'tool' ? traceDuration(r.duration_ms) : '',
      detail: r.detail || '',
      result: r.result,
      truncated: Boolean(r.truncated),
      args: r.args,
    })),
  }
}

// Folds a live frame into the rows a saved trace would have had.
export function applyTraceFrame(rows, frame) {
  if (frame.kind === 'started') {
    return [...rows, { seq: frame.seq, kind: 'tool', tool: frame.tool, args: frame.args, nested: frame.nested }]
  }
  if (frame.kind === 'finished') {
    return rows.map(r => r.seq === frame.seq
      ? { ...r, duration_ms: frame.duration_ms, status: frame.status, detail: frame.detail || undefined }
      : r)
  }
  if (frame.kind === 'event') {
    return [...rows, { seq: frame.seq, kind: 'event', detail: frame.detail, status: frame.error ? 'failed' : 'ok' }]
  }
  return rows
}
```

- [ ] **Step 4:** JS tests green.

- [ ] **Step 5: Markup and styles** in `chat.html`. After the `.turns li.scout` rule:

```css
  /* The trace under an answer: an admin's table, kept to the palette and
     to the bubble's width. Capped like the status box, for the same
     reason — a flight run has dozens of rows. */
  .turns li .trace-btn{display:block; margin-top:6px; font-size:12px; color:var(--base01);
    background:none; border:0; padding:0; cursor:pointer; text-decoration:underline dotted}
  .trace{margin-top:8px; font-size:12.5px; font-family:ui-monospace,SFMono-Regular,Menlo,monospace;
    color:var(--base1); background:var(--panel); border-radius:8px; padding:8px 10px;
    max-height:40dvh; overflow:auto}
  .trace .head{color:var(--base01); margin-bottom:6px}
  .trace .row{display:flex; gap:8px; align-items:baseline; padding:2px 0; cursor:pointer}
  .trace .row.nested{padding-left:18px}
  .trace .row.event{cursor:default; color:var(--base01)}
  .trace .row .label{flex:1; min-width:0; overflow:hidden; text-overflow:ellipsis; white-space:nowrap}
  .trace .row .dur{flex:none; color:var(--base01)}
  .trace .chip{flex:none; font-size:11px; padding:0 6px; border-radius:8px; background:#0d4a5a}
  .trace .chip.failed,.trace .row.event.failed{color:var(--orange)}
  .trace .chip.running{color:var(--cyan)}
  .trace pre{margin:4px 0 8px 18px; white-space:pre-wrap; word-break:break-word; color:var(--base1)}
```

(`--panel`, `--base01`, `--base1`, `--orange`, `--cyan` exist in the page's `:root`; check and use the names that do.)

- [ ] **Step 6: Wiring** in `chat.js` inside `start()`:

  - State: `let debugOn = false`. `async function refreshDebug() { try { const r = await fetch('/chat/debug'); if (r.ok) debugOn = (await r.json()).on } catch {} ; document.body.classList.toggle('debug', debugOn) }`. Called from the page's initial load (next to `loadHistory()`), and after a send whose text starts with `/debug`.
  - `turnElement(role, text, runId)`: when `role !== 'You'` and `runId` is a number and `debugOn`, append a `button.trace-btn` with text `Trace` and `dataset.runId = runId`; the button toggles `openTrace(li, runId)`. `showTurns` passes `turn.run_id`.
  - `renderTracePanel(li, run, rows)`: builds `div.trace` from `traceLines(run, rows)`: a `.head` div when `head` is non-empty, then per row a `div.row` with classes `tool|event`, `nested`, and `failed` when status is `failed`; children: `span.label` (textContent = label), `span.dur` (duration), `span.chip` (status or `running` when undefined). Clicking a tool row toggles a `pre` after it with `JSON.stringify(args, null, 2)` then a blank line then the result pretty-printed when it parses as JSON, else the raw text; append `\n… (cut at the store's cap)` when truncated. Use `textContent` everywhere; never `innerHTML` for trace content. Replaces any existing `.trace` in the `li`.
  - `openTrace(li, runId)`: if a `.trace` exists, remove it and return; else `fetch('/chat/runs/'+runId+'/trace')`: 403 → `showNotice('Turn debug on with /debug on to see traces.')`; 404 → render a panel whose head is `trace no longer kept`; ok → `renderTracePanel`.
  - Live: in the run's frame handler, `else if ('Trace' in evt)`: `const f = evt.Trace; if (f.kind === 'run') { liveRunId = f.run_id } else { liveRows = applyTraceFrame(liveRows, f); if (debugOn && mine()) { renderAnswer(); renderTracePanel(answerLi, null, liveRows) } }`. `liveRunId`/`liveRows` are per-send locals next to `answer`/`thinking`. When the end frame arrives and `answerLi` survives, if `debugOn` append the Trace button with `liveRunId` (so a click re-fetches the saved trace with results) and leave the live panel in place.
  - `renderAnswer` sets `answerLi.innerHTML = render(answer)`, which would wipe a live panel; change it to render the answer into a child `div.text` and keep the panel: `if (!answerLi.querySelector('.text')) answerLi.append(node('div','text'))` then set that div's innerHTML. `turnElement` renders into the same `div.text` so both paths share one structure. Check the CSS for `.turns li` does not depend on direct text children (it does not; it styles the `li`).

- [ ] **Step 7: Manual check** with the browser tool against a local run if one is available; otherwise rely on the JS tests and the source-assertion test below.

  Add to `chat.rs` tests (the file already asserts page ids exist):

```rust
    #[test]
    fn the_page_never_inserts_trace_text_as_html() {
        let js = include_str!("../chat.js");
        let start = js.find("function renderTracePanel").expect("the panel renderer must exist");
        let end = js[start..].find("\n  }\n").map(|i| start + i).unwrap_or(js.len());
        assert!(!js[start..end].contains("innerHTML"), "trace content must go through textContent");
    }
```

- [ ] **Step 8:** `node --test 'crates/scout-web/src/*.test.mjs'` and `cargo test -p scout-web` — green.

- [ ] **Step 9: Commit** `feat(web): a Trace button under every answer, live while the run streams`.

---

### Task 8: Docs

**Files:** `README.md`, `docs/BOARD.md`, `docs/superpowers/specs/2026-09-13-debug-trace-design.md`

- [ ] **Step 1: README.** In the browser section, a bullet:

```
- **A trace behind every answer.** An admin types `/debug on` and each
  Scout reply gets a Trace button: every tool the run called, with its
  arguments, how long it took, whether it failed and what it returned, the
  flight desk's nested calls indented under it, and the run's own events —
  a cut-short run, a stripped dead link. Rows stream in while the run is
  going. Traces are recorded for every run, so a bad answer from yesterday
  can be opened today; the newest 300 runs are kept.
```

In the limitations or admin section, one line: `/debug` is a web command; on Telegram the word reaches the model.

Update the test count line with the number `cargo test --workspace 2>&1 | grep -E "^test result" | awk '{s+=$4} END {print s}'` prints, plus the JS count from `node --test`.

- [ ] **Step 2: Spec.** Replace the "end frame gains run_id" sentence with: the run id reaches the page as the first trace frame, `TraceFrame::Run`. Remove `channel` from the `runs` DDL.

- [ ] **Step 3: Board.** `docs/BOARD.md`: move the "Debug trace behind every answer" line from Next to In progress (it moves to Done with the merge hash at the end).

- [ ] **Step 4: Commit** `docs: the trace behind an answer`.

---

### Task 9: Finish

- [ ] **Step 1:** `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && node --test 'crates/scout-web/src/*.test.mjs'` — all green.

- [ ] **Step 2:** Merge, push, board, deploy (the user's standing workflow):

```bash
git checkout main && git merge --no-ff feat/debug-trace -m "Merge: a trace behind every answer

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>" && git push
```

Move the board line to Done with the merge hash in the board's format (`- [x] 2026-09-13 — **Debug trace behind every answer** (\`hash\`): …`), update the artifact card, commit, push, then `scripts/deploy-k3s.sh`; confirm `scout is up` in the pod log.

- [ ] **Step 3: Live check.** As an admin on `goodscout.fyi/chat`: `/debug on`, ask a flight question, watch rows appear under the answer, click one to open its result, reload, click Trace on the same answer, then `/debug off` and confirm the buttons disappear.
