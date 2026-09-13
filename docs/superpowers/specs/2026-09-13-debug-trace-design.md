# A Trace Behind Every Answer — Design

## Purpose

When Scout answers badly, the only record of why is a log line on the
node and, on the web, nothing at all. This gives an admin a switch in the
browser, `/debug on`, that puts a trace under every answer: each tool the
run called, with its arguments, how long it took, whether it failed and
what it returned, the flight desk's nested calls indented under it, and
every run-level event — a cut-short run, a stripped dead link, a model call
that failed. Traces are recorded for every run whether or not anyone is
watching, so yesterday's bad answer can be explained today.

## Decisions taken

Settled in conversation; the rest of the document follows from them.

- **Full trace, results collapsed.** Every call with complete arguments and
  the complete result, shown one line per call and opened on click.
- **A panel under each answer**, not a drawer for the thread. Past turns
  get one too, from what was recorded.
- **Per account, server-side, admins only.** `/debug on` and `/debug off`
  are handled by the server and stored on the account; anyone whose account
  has no Telegram identity in `SCOUT_ADMIN_USER_IDS` is told it is an admin
  switch.
- **Record a compact trace for every run, always.** The flag controls
  display, not recording.

## Data

Migration step 12, and 13 for the constraint, in `store.rs`, numbered after
the kept-trips steps that shipped on 2026-09-10.

```sql
-- step 12
CREATE SEQUENCE IF NOT EXISTS runs_id_seq;
CREATE TABLE IF NOT EXISTS runs (
    id              BIGINT PRIMARY KEY DEFAULT nextval('runs_id_seq'),
    account_id      BIGINT NOT NULL,
    conversation_id BIGINT NOT NULL,
    channel         TEXT NOT NULL,
    started_at      TIMESTAMP NOT NULL DEFAULT current_timestamp,
    ended_at        TIMESTAMP,
    outcome         TEXT,          -- answered | cut_short | failed
    detail          TEXT           -- the reason or the error sentence
);
CREATE SEQUENCE IF NOT EXISTS run_traces_id_seq;
CREATE TABLE IF NOT EXISTS run_traces (
    id          BIGINT PRIMARY KEY DEFAULT nextval('run_traces_id_seq'),
    run_id      BIGINT NOT NULL,
    seq         BIGINT NOT NULL,   -- order within the run
    kind        TEXT NOT NULL,     -- tool | event
    tool        TEXT,              -- tool rows
    args        TEXT,              -- JSON, tool rows
    nested      BOOLEAN NOT NULL DEFAULT false,
    started_at  TIMESTAMP NOT NULL,
    duration_ms BIGINT,
    status      TEXT,              -- ok | failed, tool rows
    detail      TEXT,              -- error text, or the event's wording
    result      TEXT,              -- the tool's result text, capped
    truncated   BOOLEAN NOT NULL DEFAULT false
);
ALTER TABLE messages ADD COLUMN IF NOT EXISTS run_id BIGINT;
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS debug BOOLEAN;
UPDATE accounts SET debug = false WHERE debug IS NULL;
ALTER TABLE accounts ALTER COLUMN debug SET DEFAULT false;
-- step 13
ALTER TABLE accounts ALTER COLUMN debug SET NOT NULL;
```

Same shape as steps 7 and 8, for the same DuckDB reasons: `ADD COLUMN`
cannot carry a constraint, and `SET NOT NULL` refuses to share a
transaction with the rows it just touched.

- `result` is capped at `TRACE_RESULT_CAP = 64 * 1024` bytes; a longer one
  is cut at a character boundary and `truncated` set. Storing the result on
  the row rather than joining back to `messages` keeps nested calls, whose
  transcript is thrown away, on the same footing as top-level ones. It
  duplicates top-level results the messages table already holds; accepted.
- Retention: `trim_traces(keep_runs = 300)` runs in the hourly maintenance
  next to `trim_message_logs`, deleting `runs` and `run_traces` for
  everything but the newest 300 runs. `messages.run_id` may then point at
  a deleted run; the trace endpoint answers 404 and the page says "trace
  no longer kept".
- `Turn` on the wire gains `run_id: Option<i64>`, set on Scout turns from
  `messages.run_id`; `None` on You turns and on messages saved before this
  change. `Store::conversation_messages` returns `(run_id, body)` for it.

## Recording

### `Observer` (new, `observer.rs` in scout-core)

The three things a run hands to its agent — the event sink, the pulse and
now the trace — become one struct behind an `Arc`:

```rust
pub struct Observer {
    pub events: EventSink,
    pub pulse: Pulse,
    pub run_id: i64,
    rows: Mutex<Vec<TraceRow>>,
    open: Mutex<HashMap<String, usize>>,   // call_id -> row index
}
```

- `tool_started(call_id, tool, args, nested)` pushes a `TraceRow` with
  `started_at = now`, remembers its index by `call_id`, and emits
  `AgentEvent::Trace(TraceFrame::Started { seq, tool, args, nested })`.
- `tool_finished(call_id, result_text)` fills `duration_ms`, `status`
  (`ok` when the text parses as JSON, `failed` otherwise — the rule the
  flight desk already uses), `result` (capped) and `detail` (the text when
  failed), and emits `Trace(Finished { seq, duration_ms, status, detail })`.
- `event(detail, is_error)` pushes an event row and emits
  `Trace(Event { seq, detail, error })`.
- `take_rows()` hands the rows over for saving.

`build_agent(d, run, facts, observer: Arc<Observer>)` replaces the
`events, pulse` pair; `ask_flights` and `Specialist` hold the same `Arc`.
The specialist's collector calls `tool_started(.., nested = true)` and
`tool_finished` beside its own bookkeeping, so nested rows are recorded by
the same code; its own `ask_flights` row is recorded by the outer loop like
any tool.

### `run.rs`

- `Store::open_run(account_id, conversation_id, channel) -> run_id` right
  after the slot is taken, before the agent is built. Busy and overloaded
  runs never reach it and leave no row.
- The outer loop already handles `ToolExecutionStart`; it also handles
  `StreamUserItem(ToolResult)` and calls `tool_finished` with the result
  text (the same extraction the specialist does).
- Every site that logs a warning about the run records an event row with
  the same wording: the model call failed; run interrupted, writing up from
  notes; the model wrote a tool call as text; dead links in reply, asking
  the agent to correct; dead links survived the correction, stripping.
- `append_history` takes the run id and writes it on every message it
  inserts. Then `append_traces(run_id, rows)` and
  `close_run(run_id, outcome, detail)`. Order: messages, traces, run.
- `RunOutcome::Answered` carries the run id alongside the answer so the
  web end frame can name it.

### `AgentEvent::Trace`

A new variant carrying `TraceFrame` (`Started`, `Finished`, `Event`), all
plain data. The web route serialises it under `event: agent` as it does
every event; the Telegram renderer's `match` ignores it. Frames go out
regardless of the flag; a page with debug off drops them.

Provider retries are below the tool boundary and are not recorded; when
they run out, the tool's final error is the row's `detail`.

## The command and the API

- **`/debug on`, `/debug off`, `/debug`** are intercepted in the web
  `send_message` before the daily cap and before any run. Admin means: any
  Telegram id of the account is in `SCOUT_ADMIN_USER_IDS`
  (`Store::telegram_ids` + `Core::is_admin`). A non-admin gets a stream
  ending `{"status":"ok","answer":"That's an admin switch."}` and nothing
  is spent, logged or stored. An admin's `on`/`off` calls
  `Store::set_debug(account_id, bool)` and ends with "Debug is on. Each
  answer now shows its trace." or "Debug is off."; bare `/debug` answers
  the current state. Nothing enters the conversation, so the exchange
  vanishes on reload. On Telegram `/debug` is not a command and reaches the
  model as text.
- **`GET /chat/debug`** → `{"on": bool}` for the caller's account.
- **`GET /chat/runs/{id}/trace`** → `{"run": {...}, "rows": [...]}`, rows in
  `seq` order with results. 404 when the run is not the caller's or no
  longer kept; 403 when the account's debug is off. `Store::trace_of(run_id,
  account_id)` does the ownership check in SQL.
- **The end frame** `End::Ok` gains `run_id: Option<i64>`.

## The page

- **A trace button** (`Trace`) under every Scout turn with a run id, shown
  only while debug is on. Click fetches the trace and opens a panel under
  the bubble; click again closes it. Turns without a run id show nothing;
  a 404 shows "trace no longer kept".
- **The panel** heads with the run row — outcome, total time, detail when
  cut short — then one row per trace entry in `seq` order. A tool row: name,
  arguments on one line (cut at 120 chars), duration, a status chip. Nested
  rows are indented. An event row spans the width and is marked as an error
  when it is one. Clicking a tool row expands it: arguments and result
  pretty-printed as JSON, or the error text; a truncated result says so.
- **Live.** While a run streams with debug on, the panel opens under the
  answer in progress. A `Started` frame adds a row with a running timer, a
  `Finished` frame fills it in, an `Event` frame adds a line. When the
  stream ends the panel stays and the button takes the run id from the end
  frame, so a click re-fetches the saved trace with results.
- **Rendering** uses the page's helpers: `escapeHtml`, then `linkify`;
  nothing from tool output is inserted as HTML. The panel is capped at
  40vh and scrolls inside itself.

## Testing

- **Store:** migration from a version-11 fixture with rows: the two tables,
  `messages.run_id`, `accounts.debug` with the backfill; `open_run`,
  `close_run`, `append_traces`, `trace_of` refusing another account's run,
  `set_debug`/`debug_of`, `trim_traces` keeping 300, a result over the cap
  stored cut with `truncated`.
- **Observer:** start then finish yields one row with duration and status;
  a non-JSON result is `failed` with the text as `detail`; an event row
  carries its wording; each call emits a `Trace` frame; nested rows carry
  `nested = true`.
- **Run loop:** source assertions that every warning site records an
  event, that `append_history` receives the run id, and that traces are
  saved after messages and before the run closes.
- **Session:** `turns_of` puts the run id on a Scout turn and none on a You
  turn or a pre-migration message.
- **Web:** `/debug on` from a non-admin spends nothing and changes nothing;
  from an admin flips the flag without a run; `GET /chat/debug` reflects
  it; the trace endpoint's 404 and 403; the end frame carries the run id.
- **Client (`chat.test.mjs`):** a pure `traceRows(run, rows)` producing panel
  rows with nesting and status; `Started` then `Finished` frames yielding
  one finished row; the flag hiding every trace button.

After merge: `/debug on` as an admin, ask a flight question, watch the
panel fill live, reload, reopen the same trace.

## Out of scope

Traces on Telegram; per-thread or per-run toggles; recording provider
retries; token usage per call (the board's token card).
