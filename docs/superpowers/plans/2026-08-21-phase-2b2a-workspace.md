# Phase 2b-2a — The Workspace Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the hand-drawn boundary from 2b-1 into three cargo crates, so that reaching from the Telegram adapter into the database becomes a compile error rather than a code review.

**Architecture:** `scout-api` holds the event protocol both sides speak. `scout-core` is a library holding everything that answers a question — and it does *not* export `Store` or `AgentDeps`, which is what makes the boundary real. `scout-telegram` is the only binary: teloxide, `Live`, chunking, routing, delivery. One process still, one container still, identical behaviour.

**Tech Stack:** Rust 2021, cargo workspace, existing deps unchanged. No new runtime dependency is added by this phase.

---

## Why this phase exists at all

2b-1 moved the logic and got the adapter to zero store calls. Nothing enforces
that. A future edit can write `app.core.store()` in `bot.rs` and the compiler
will be perfectly happy — which is how the `/stat` id-space bug survived
review in the first place. This phase makes the class of mistake unavailable.

The other reason is 2b-2b. Splitting a process is much easier when the crate
graph already says which side each function is on.

## Verified before writing this plan

Measured against the tree at `c051f70`, not assumed:

| Claim | How it was checked | Result |
|---|---|---|
| Only four files touch teloxide | `grep -rln teloxide src/` | `progress.rs`, `main.rs`, `bot.rs`, `scheduler.rs` |
| Core code reaches into the adapter exactly once | `grep -rn 'crate::progress::\|crate::bot::' src/` excluding those files | one hit: `src/run.rs:77` calls `progress::describe` |
| `bot.rs` never names an event | `grep -c crate::events src/bot.rs` | `0` — it goes through `render_events` |
| Event references to update | `grep -c crate::events` | `run.rs` 10, `progress.rs` 8 |
| `tools/` is teloxide-free | `grep -rln teloxide src/tools/` | no matches |
| The scheduler needs both halves | read `src/scheduler.rs:24-60` | takes `Bot` *and* `Store`; must split |
| Reminders already carry a channel and address | phase-one migration, step 3 | `reminder.address`, `reminder.channel` exist |
| Baseline to preserve | `cargo test` | 438 passed, 3 ignored |

Two consequences that shape the tasks:

- `describe` has to move to core before the crates split, or `scout-core`
  would depend on `scout-telegram`. That is Task 2, and it is why it comes
  before the move.
- The scheduler has to split in this phase, not in 2b-2b. Once `Store` is
  private, an adapter-side reminder loop cannot compile.

## File Structure

```
scout/
  Cargo.toml                       workspace manifest only, after Task 6
  crates/
    scout-api/
      Cargo.toml
      src/lib.rs                   AgentEvent, EventSink, emit  (from src/events.rs)
    scout-core/
      Cargo.toml
      src/lib.rs                   module list; store and agent are PRIVATE
      src/{core,session,invites,run,stats,store,agent,config}.rs
      src/{vision,links,text,draft,describe}.rs
      src/tools/                   unchanged
    scout-telegram/
      Cargo.toml
      src/main.rs                  startup, wiring
      src/bot.rs                   routing, gate, chunking, delivery
      src/progress.rs              Live, pacing, flood control
      src/scheduler.rs             the 15-minute loop and the sending
```

`describe` ends up in `scout-core/src/describe.rs`: it renders a tool call as
a human sentence, and every channel wants the same sentence.

---

### Task 1: The workspace manifest and the event protocol crate

**Files:**
- Modify: `Cargo.toml` (add a `[workspace]` section and the new dependency)
- Create: `crates/scout-api/Cargo.toml`
- Create: `crates/scout-api/src/lib.rs`
- Delete: `src/events.rs`
- Modify: `src/main.rs:6` (drop `mod events;`), `src/run.rs` (10 references), `src/progress.rs` (8 references)

- [ ] **Step 1: Create the crate manifest**

`crates/scout-api/Cargo.toml`:

```toml
[package]
name = "scout-api"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "The events Scout's core emits and its channels render"

[dependencies]
serde = { version = "1", features = ["derive"] }
tokio = { version = "1", features = ["sync"] }

[dev-dependencies]
serde_json = "1"
```

- [ ] **Step 2: Move the event protocol into it**

`git mv src/events.rs crates/scout-api/src/lib.rs` (create the directory
first with `mkdir -p crates/scout-api/src`).

Then add the derives that make it a wire type. The enum keeps its variants
and its doc comments; only the derive line changes:

```rust
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AgentEvent {
```

- [ ] **Step 3: Write the failing test**

Append to the existing `mod tests` in `crates/scout-api/src/lib.rs`. This is
the test that justifies the crate existing: both binaries will parse what the
other one wrote.

```rust
    #[test]
    fn an_event_survives_the_json_round_trip_both_sides_will_use() {
        // In 2b-2b this crosses a socket. The point of one shared crate is
        // that the two ends cannot disagree about what an event is.
        let each_kind = vec![
            AgentEvent::Tool("🔎 searching: wasmiddel".to_string()),
            AgentEvent::Answer("The cheapest is".to_string()),
            AgentEvent::Thinking("comparing fares".to_string()),
            AgentEvent::Notice("wrapped up early".to_string()),
        ];
        for event in each_kind {
            let wire = serde_json::to_string(&event).unwrap();
            let back: AgentEvent = serde_json::from_str(&wire).unwrap();
            assert_eq!(back, event, "round trip changed the event: {wire}");
        }
    }
```

- [ ] **Step 4: Wire the workspace up**

In the root `Cargo.toml`, add above `[dependencies]`:

```toml
[workspace]
resolver = "2"
members = ["crates/scout-api"]
```

`resolver = "2"` is not optional: a workspace defaults to resolver 1 even when
its members are edition 2021, and cargo warns about the mismatch on every
build.

Add to the root `[dependencies]`:

```toml
scout-api = { path = "crates/scout-api" }
```

- [ ] **Step 5: Point the two consumers at the new crate**

Delete `mod events;` from `src/main.rs`. Then in `src/run.rs` and
`src/progress.rs`, replace every `crate::events::` with `scout_api::`:

```bash
sed -i '' 's/crate::events::/scout_api::/g' src/run.rs src/progress.rs
```

Check nothing else referred to it:

```bash
grep -rn "crate::events" src/ && echo "STILL REFERENCED" || echo "clean"
```

Expected: `clean`.

- [ ] **Step 6: Run the tests**

Run: `cargo test`
Expected: two test binaries now — `scout-api` reporting its 3 tests
(`emitting_into_a_closed_channel_is_not_an_error`,
`events_arrive_in_the_order_they_were_sent`, and the new round trip) and
`scout` reporting 436. Total 439 passed, 3 ignored.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "refactor: the event protocol becomes a crate both sides can depend on"
```

---

### Task 2: `describe` moves to the side that produces it

**Files:**
- Create: `src/describe.rs`
- Modify: `src/progress.rs` (delete lines 229-271 and two tests), `src/run.rs:77`, `src/main.rs` (add `mod describe;`)

`run.rs` calling into `progress.rs` is the one edge pointing the wrong way.
It has to go before the crates split or `scout-core` would depend on
`scout-telegram`.

- [ ] **Step 1: Move the functions**

Create `src/describe.rs` containing `describe` (`src/progress.rs:229-264`)
and `host` (`src/progress.rs:266-271`) verbatim, with this module doc on top:

```rust
//! How a tool call reads to a person.
//!
//! This lives with the agent rather than with the renderer because every
//! channel wants the same sentence: a browser should not invent its own
//! wording for "opening bol.com". The event carries the finished text.
```

`describe` stays `pub`; `host` stays private to the module.

- [ ] **Step 2: Move the two tests that cover them**

Move `tool_calls_read_as_plain_progress` (`src/progress.rs:278`) and
`missing_or_odd_arguments_never_panic` (`src/progress.rs:305`) into a
`#[cfg(test)] mod tests` in `src/describe.rs`, with
`use serde_json::json;`. They test `describe` only, so they transfer
unchanged.

- [ ] **Step 3: Update the caller and register the module**

`src/run.rs:77`: `crate::progress::describe(` becomes `crate::describe::describe(`.
`src/main.rs`: add `mod describe;` in alphabetical position (after `mod core;`).

- [ ] **Step 4: Verify the wrong-way edge is gone**

```bash
grep -rn "crate::progress::\|crate::bot::" src/ | grep -v "^src/bot.rs\|^src/progress.rs"
```

Expected: no output. This is the check that makes Task 3 possible.

- [ ] **Step 5: Run the tests**

Run: `cargo test`
Expected: 439 passed, 3 ignored. The same tests, in a different binary.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor: naming a tool call is the agent's job, not the renderer's"
```

---

### Task 3: `scout-core` becomes a crate

**Files:**
- Create: `crates/scout-core/Cargo.toml`, `crates/scout-core/src/lib.rs`
- Move: thirteen modules and `src/tools/` into `crates/scout-core/src/`
- Modify: root `Cargo.toml`, `src/main.rs`, `src/bot.rs`

The largest task, and almost entirely mechanical: paths *inside* the moved
code stay `crate::`, because the moved code all lands in the same new crate.
Only the adapter's references change.

- [ ] **Step 1: Move the files**

```bash
mkdir -p crates/scout-core/src
git mv src/agent.rs src/config.rs src/core.rs src/describe.rs src/draft.rs \
       src/invites.rs src/links.rs src/run.rs src/session.rs src/stats.rs \
       src/store.rs src/text.rs src/vision.rs crates/scout-core/src/
git mv src/tools crates/scout-core/src/tools
```

- [ ] **Step 2: Write the crate manifest**

`crates/scout-core/Cargo.toml` — the root manifest's dependency list minus
teloxide, tracing-subscriber and dotenvy, which are the binary's business:

```toml
[package]
name = "scout-core"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "Everything that answers a question, with no idea who asked"

[dependencies]
scout-api = { path = "../scout-api" }
rig = "0.40"
tokio = { version = "1", features = ["io-util", "macros", "process", "rt-multi-thread", "time"] }
duckdb = { version = "1", features = ["bundled"] }
dashmap = "6"
reqwest = { version = "0.12", features = ["json"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
thiserror = "2"
chrono = "0.4"
futures = "0.3"
base64 = "0.22"
tracing = "0.1"

[dev-dependencies]
wiremock = "0.6"
tempfile = "3"
tokio = { version = "1", features = ["test-util"] }
```

- [ ] **Step 3: Write the library root**

`crates/scout-core/src/lib.rs`:

```rust
//! Everything that answers a question, and nothing about who asked it.
//!
//! `store` and `agent` become private in Task 5, once nothing outside this
//! crate names them. A channel that can name `Store` can query it, and the
//! boundary goes back to being a convention.

pub mod agent;
pub mod store;

pub mod config;
pub mod core;
pub mod describe;
pub mod draft;
pub mod invites;
pub mod links;
pub mod run;
pub mod session;
pub mod stats;
pub mod text;
pub mod tools;
pub mod vision;
```

Leave `config` public for now — Task 4 removes the adapter's last use of it,
and 2b-2b divides it properly.

- [ ] **Step 4: Add the dependency and trim the root manifest**

Root `Cargo.toml`: add `scout-core = { path = "crates/scout-core" }`, extend
`members` to `["crates/scout-api", "crates/scout-core"]`, and delete the
dependencies that moved: `rig`, `duckdb`, `thiserror`, `futures`, `base64`.

Keep `reqwest` for now. `main.rs:40` still builds the shared HTTP client
itself, and that construction does not move until Task 4 — deleting the
dependency here would break the build for no reason. Task 4 removes it.

Also keep `teloxide`, `tokio`, `dashmap`, `dotenvy`, `serde`, `serde_json`,
`anyhow`, `chrono`, `tracing`, `tracing-subscriber`.

- [ ] **Step 5: Point the adapter at the crate**

```bash
sed -i '' -E 's/crate::(agent|config|core|describe|draft|invites|links|run|session|stats|store|text|tools|vision)::/scout_core::\1::/g' \
    src/main.rs src/bot.rs src/scheduler.rs src/progress.rs
```

Then fix the `use` lines at the top of `src/main.rs` by hand: `use
agent::AgentDeps;`, `use config::Config;` and `use store::Store;` become
`scout_core::` paths, and the `mod` declarations for the moved modules are
deleted, leaving only `mod bot; mod progress; mod scheduler;`.

- [ ] **Step 6: Build**

Run: `cargo build --workspace 2>&1 | head -40`

This must succeed. Every module is `pub` for now, so the move is the only
change and nothing has been taken away yet — `main.rs` still constructs
`Store` and `AgentDeps` exactly as it does today, just through
`scout_core::` paths. An error here means a path was rewritten wrongly.

The tightening comes later on purpose: Task 4 removes `main.rs`'s reasons to
name those types, Task 5 removes the scheduler's, and only then does Task 5
make the modules private. Privacy applied before the last caller is gone
would mean two commits that do not build, which is a bad way to bisect.

- [ ] **Step 7: Run the tests and commit**

Run: `cargo test --workspace`
Expected: 439 passed, 3 ignored — the same tests, now spread across three
crates.

```bash
git add -A
git commit -m "refactor: core becomes a crate"
```

---

### Task 4: The store stops being reachable

**Files:**
- Modify: `crates/scout-core/src/core.rs` (add `start`, `members`, accessors)
- Modify: `src/main.rs` (loses ~90 lines of client construction)

- [ ] **Step 1: Write the failing test**

In `crates/scout-core/src/core.rs`, in `mod tests`:

```rust
    #[test]
    fn a_core_opens_its_own_database_and_never_hands_it_out() {
        // The whole point of the crate split: a channel can start a core
        // but cannot reach past it. If `Store` ever becomes nameable from
        // outside, this test still passes — but `cargo build -p
        // scout-telegram` is what actually enforces it, and Task 8 measures
        // it. This test pins the constructor's contract: it opens the file
        // itself, from config, with no handle passed in.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("start.duckdb");
        let cfg = Config::for_test(path.to_str().unwrap());

        let core = Core::start(cfg, None).unwrap();

        assert_eq!(core.members().unwrap(), Vec::<i64>::new());
        assert!(core.schema_version().unwrap() >= 5);
    }
```

This needs a test constructor for `Config`. `from_lookup`
(`src/config.rs:54`) already takes an arbitrary environment, so the
constructor is four required variables and nothing else — every optional
integration then resolves to `None`, which is a state production also runs in:

```rust
impl Config {
    /// A Config with only the four required variables set and the database
    /// somewhere temporary. Mirrors `base_env()` in this module's tests.
    #[cfg(test)]
    pub fn for_test(db_path: &str) -> Self {
        Self::from_lookup(|k| match k {
            "TELEGRAM_BOT_TOKEN" => Some("tok".to_string()),
            "ALLOWED_TELEGRAM_USER_IDS" => Some("111".to_string()),
            "MINIMAX_API_KEY" => Some("mk".to_string()),
            "KAGI_API_KEY" => Some("kk".to_string()),
            "SCOUT_DB_PATH" => Some(db_path.to_string()),
            _ => None,
        })
        .expect("the four required variables are all set")
    }
}
```

Check the database variable's real name against `config.rs` before writing
this — the README documents it as `SCOUT_DB_PATH`.

- [ ] **Step 2: Run it to watch it fail**

Run: `cargo test -p scout-core a_core_opens_its_own_database`
Expected: FAIL — `no function or associated item named 'start' found`.

- [ ] **Step 3: Move the wiring into `Core::start`**

Cut `src/main.rs:38-127` — everything from `Store::open` through the
`AgentDeps` literal — into `crates/scout-core/src/core.rs` as:

```rust
impl Core {
    /// Opens the database and builds every client the agent can use.
    ///
    /// `return_url` is where Duffel Links sends a traveller back to, and it
    /// is the one thing core cannot work out for itself: today it comes from
    /// the bot's own `getMe`. When core moves into its own process it will
    /// have to be configured or asserted at start-up — see the spec.
    pub fn start(cfg: Config, return_url: Option<String>) -> anyhow::Result<Self> {
        let store = Store::open(&cfg.db_path)?;
        // ... the body of main.rs:40-127, unchanged, with `deps.return_url`
        // set from the argument instead of being filled in afterwards.
        Ok(Self { cfg, deps })
    }
}
```

The `tracing::info!` lines about which integrations are live move with the
code they describe. They belong to core: whether Duffel is configured is not
the adapter's news to report.

- [ ] **Step 4: Add the three things the adapter still needs**

```rust
impl Core {
    /// Everyone currently admitted, as Telegram ids, for the adapter's gate.
    ///
    /// A read at start-up and nothing more: the gate runs on every update,
    /// including from strangers, so it must not touch the database.
    pub fn members(&self) -> anyhow::Result<Vec<i64>> {
        self.deps.store.active_members()
    }

    /// For the start-up log line.
    pub fn schema_version(&self) -> anyhow::Result<i64> {
        self.deps.store.schema_version()
    }

    /// Founders, admins and the cap, for the same log line.
    pub fn population(&self) -> (usize, usize, i64) {
        (
            self.cfg.allowed_user_ids.len(),
            self.cfg.admin_user_ids.len(),
            self.cfg.invite_daily_requests,
        )
    }
}
```

`invite_daily_requests` is an `i64` (`src/config.rs:5-46`), not a `u32`.

- [ ] **Step 5: Make the fields private**

In `crates/scout-core/src/core.rs`, `pub cfg` and `pub deps` become plain
`cfg` and `deps`. Fix whatever inside `scout-core` referenced them through
the struct — all of it is in the same crate, so it keeps compiling.

- [ ] **Step 6: Rewrite `main.rs` around the new constructor**

What is left is short. The order matters: the bot exists first because
`get_me` supplies `return_url`.

```rust
    let cfg = Config::from_env()?;
    let telegram = Bot::new(telegram_token()?);

    let return_url = match teloxide::prelude::Requester::get_me(&telegram).await {
        Ok(me) => me.username.as_ref().map(|u| format!("https://t.me/{u}")),
        Err(e) => {
            tracing::warn!(error = %e, "could not read the bot's username; booking links disabled");
            None
        }
    };

    let core = Arc::new(scout_core::core::Core::start(cfg, return_url)?);

    let members: dashmap::DashSet<i64> = core.members()?.into_iter().collect();
    let (founders, admins, daily_cap) = core.population();
    tracing::info!(founders, admins, members = members.len(), daily_cap,
        schema = core.schema_version()?, "who may talk to this bot");
```

`Config::from_env` still parses the token, and the adapter now reads it
directly, because a core that hands out a bot token is a core that knows what
Telegram is. Add to `src/main.rs`:

```rust
/// The adapter's own credential, read straight from the environment.
///
/// 2b-2b divides `Config` in two and this is the first piece to move.
fn telegram_token() -> Result<String> {
    std::env::var("TELEGRAM_BOT_TOKEN")
        .map_err(|_| anyhow::anyhow!("TELEGRAM_BOT_TOKEN is not set"))
}
```

Use the variable name `config.rs` actually reads — check
`crates/scout-core/src/config.rs` before writing this, and match it exactly.

- [ ] **Step 7: Run the tests**

Run: `cargo test`
Expected: 440 passed, 3 ignored, split across three test binaries.

Then confirm the boundary holds where it counts:

```bash
grep -n "Store\|AgentDeps\|duckdb\|kagi\|rig::" src/*.rs
```

Expected: `src/scheduler.rs` and nothing else. It legitimately still holds a
`Store` — Task 5 is what removes that, and Task 5 is what then makes the
module private.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "refactor: a channel can start a core but cannot reach past it"
```

---

### Task 5: The scheduler splits along the same line

**Files:**
- Modify: `crates/scout-core/src/core.rs` (add `due_deliveries`, `delivery_done`)
- Create: `crates/scout-core/src/schedule.rs` (`advance_from` and the date maths)
- Modify: `src/scheduler.rs` (keeps the loop and the sending only)

Deciding a reminder is due is core's work; sending it is a channel's. The two
methods below are deliberately shaped like the endpoints that will wrap them:
`GET /v1/deliveries?channel=telegram` and `POST /v1/deliveries/{id}/ack`.

- [ ] **Step 1: Write the failing test**

In `crates/scout-core/src/core.rs`, `mod tests`:

```rust
    #[tokio::test]
    async fn a_due_reminder_arrives_addressed_and_worded_ready_to_send() {
        // The channel is told where to send and what to say. It is not told
        // the interval, the next date, or anything else it would have to do
        // arithmetic on — that stays here, so a second channel cannot get
        // the cadence subtly wrong.
        let dir = tempfile::tempdir().unwrap();
        let core = Core::start(
            Config::for_test(dir.path().join("due.duckdb").to_str().unwrap()),
            None,
        )
        .unwrap();

        // The test is inside scout-core, so it may use the store directly —
        // that is precisely the privilege the adapter no longer has.
        let store = core.store();
        let account = store.account_for_telegram(4242).unwrap();
        store
            .create_reminder(account, "telegram", "4242", "detergent", 30, "2020-01-01")
            .unwrap();

        let due = core.due_deliveries("telegram").await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].address, "4242");
        assert!(due[0].text.contains("detergent"), "got: {}", due[0].text);

        core.delivery_done(due[0].id).await.unwrap();
        assert!(core.due_deliveries("telegram").await.unwrap().is_empty());
    }
```

`create_reminder(account_id, channel, address, item, interval_days,
next_due)` is `src/store.rs:749` and needs no wrapper.

- [ ] **Step 2: Run it to watch it fail**

Run: `cargo test -p scout-core a_due_reminder_arrives_addressed`
Expected: FAIL — `no method named 'due_deliveries'`.

- [ ] **Step 3: Implement the two methods**

```rust
/// One thing to deliver, to one address, on one channel.
///
/// Everything a channel needs and nothing more. `text` is written here so
/// that a browser and a chat say the same sentence.
pub struct DueDelivery {
    pub id: i64,
    pub channel: String,
    pub address: String,
    pub text: String,
}
```

`due_deliveries(channel)` reads `store.due_reminders(today)` — today computed
inside core with `chrono::Local::now().date_naive()` — filters to the channel,
skips rows core would refuse to act on, and formats the text. The three
guards currently in `src/scheduler.rs:41-60` move here unchanged: an
`interval_days < 1`, an unparseable address, and an unparseable `next_due` are
all logged and skipped.

`delivery_done(id)` advances `next_due` by whole intervals using
`advance_from`, which moves to `crates/scout-core/src/schedule.rs` with its
tests. Not acking leaves the row untouched, which is how a failed send retries
on the next tick — the behaviour `src/scheduler.rs` has today.

- [ ] **Step 4: Run it to watch it pass**

Run: `cargo test -p scout-core a_due_reminder_arrives_addressed`
Expected: PASS

- [ ] **Step 5: Reduce the adapter's scheduler to a loop and a send**

`src/scheduler.rs` keeps `TICK`, `run` and `tick`. `tick` becomes:

```rust
async fn tick(bot: &Bot, core: &Core) -> Result<()> {
    for delivery in core.due_deliveries("telegram").await? {
        let Ok(chat) = delivery.address.parse::<i64>() else {
            tracing::error!(id = delivery.id, address = %delivery.address,
                "unparseable telegram address; skipping");
            continue;
        };
        match bot.send_message(ChatId(chat), &delivery.text).await {
            Ok(_) => core.delivery_done(delivery.id).await?,
            Err(e) => tracing::warn!(id = delivery.id, error = %e,
                "reminder send failed; it stays due"),
        }
    }
    Ok(())
}
```

`advance_from`, `chrono`, `NaiveDate` and the reminder wording all leave this
file. Update `src/main.rs:154` to
`tokio::spawn(scheduler::run(telegram.clone(), core.clone()))`, which also
removes `main.rs`'s last mention of `store`.

- [ ] **Step 6: Shut the door**

Nothing outside `scout-core` names `Store` or `AgentDeps` any more, so in
`crates/scout-core/src/lib.rs`:

```rust
mod agent;
mod store;
```

`Core::store()` becomes `pub(crate)` in the same edit, or the private module
leaks straight back out through the return type — the compiler says so
plainly, `private type in public interface`.

This is the commit the whole phase exists for. Everything before it moved
code; this line is what makes the boundary hold.

- [ ] **Step 7: Verify the adapter is clean**

```bash
grep -rn "Store\|AgentDeps\|duckdb\|chrono" src/
```

Expected: no output.

- [ ] **Step 8: Run the tests and commit**

Run: `cargo test`
Expected: 441 passed, 3 ignored.

```bash
git add -A
git commit -m "refactor: core decides a reminder is due, the channel delivers it"
```

---

### Task 6: The root package becomes `crates/scout-telegram`

**Files:**
- Move: `src/` → `crates/scout-telegram/src/`
- Rewrite: `Cargo.toml` as a workspace manifest
- Create: `crates/scout-telegram/Cargo.toml`

- [ ] **Step 1: Move**

```bash
mkdir -p crates/scout-telegram
git mv src crates/scout-telegram/src
```

- [ ] **Step 2: Split the manifest in two**

`crates/scout-telegram/Cargo.toml` takes the current root package's
`[dependencies]` and `[dev-dependencies]` verbatim, with:

```toml
[package]
name = "scout-telegram"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "Scout's Telegram channel"
repository = "https://github.com/watchcat/scout"
```

The root `Cargo.toml` becomes only:

```toml
[workspace]
resolver = "2"
members = ["crates/scout-api", "crates/scout-core", "crates/scout-telegram"]
```

- [ ] **Step 3: Run the tests**

Run: `cargo test`
Expected: 441 passed, 3 ignored. `Cargo.lock` changes — the package renamed —
and stays at the repository root.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "refactor: the binary is the telegram channel, and is named that"
```

---

### Task 7: The build follows the move

**Files:**
- Modify: `Dockerfile:15-22, 36, 41`
- Modify: `scripts/deploy.sh:41-43`
- `compose.yaml` is unchanged: still one service, still named `scout`

- [ ] **Step 1: Update the Dockerfile**

```dockerfile
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/app/target,sharing=locked \
    cargo build --release \
    && cp target/release/scout-telegram /scout-telegram
```

and at the bottom:

```dockerfile
COPY --from=builder /scout-telegram /usr/local/bin/scout-telegram
...
CMD ["scout-telegram"]
```

The cache-mount comment above the build stays true and stays where it is.

- [ ] **Step 2: Update the deploy script's dirty check**

`scripts/deploy.sh:41` and `:43` both list the paths that matter to a build.
Replace `src` with `crates` in each:

```bash
if [ -n "$(git status --porcelain -- crates Cargo.toml Cargo.lock compose.yaml Dockerfile)" ]; then
```

- [ ] **Step 3: Dry-run the deploy**

Run: `scripts/deploy.sh --dry-run`
Expected: it prints the plan and reports a clean tree. If it reports the tree
dirty, the path list is still wrong.

- [ ] **Step 4: Build the image for real**

Run: `docker compose build`
Expected: success. The DuckDB C++ compile should *not* re-run — the cache
mount is keyed on the dependency set, which has not changed. A ten-minute
build here means something moved that should not have.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "build: the image builds a workspace"
```

---

### Task 8: Measure the boundary, then deploy

- [ ] **Step 1: Measure what the crate split bought**

```bash
echo "adapter lines:      $(cat crates/scout-telegram/src/*.rs | wc -l)"
echo "adapter deps:       $(cargo tree -p scout-telegram --depth 1 | tail -n +2 | wc -l)"
echo "duckdb in adapter:  $(grep -rc duckdb crates/scout-telegram/src/ | grep -v ':0' | wc -l)"
echo "store in adapter:   $(grep -rn 'Store' crates/scout-telegram/src/ | wc -l)"
```

Expected: `duckdb in adapter` and `store in adapter` both `0`. Record the
numbers in the completion report; they are the evidence the phase worked.

- [ ] **Step 2: Prove the boundary is enforced, not just observed**

Temporarily add to `crates/scout-telegram/src/main.rs`:

```rust
let _: scout_core::store::Store = todo!();
```

Run: `cargo build -p scout-telegram 2>&1 | head -5`
Expected: `error[E0603]: module 'store' is private`.

**Remove the line again.** This is a mutation check, not a change — the same
discipline as breaking a guard to watch a test go red.

- [ ] **Step 3: Full verification**

```bash
cargo test              # 441 passed, 3 ignored
cargo clippy --all-targets   # silent
```

- [ ] **Step 4: Back up the database from inside the container**

```bash
docker compose exec scout cp /data/scout.duckdb /data/scout.duckdb.pre-2b2a
```

Never from the host: the host's DuckDB is 1.4.5 and the container's is 1.5.1.
Older, and it must not write to that file.

This phase runs no migration, so the backup is precaution rather than
necessity — take it anyway.

- [ ] **Step 5: Deploy**

Run: `scripts/deploy.sh`
Expected: `scout is up`, the `who may talk to this bot` line reporting
`schema=5` and the same founder and member counts as before.

- [ ] **Step 6: Verify against production**

```bash
docker compose ps                       # restarts should be 0
docker compose logs --since 5m | grep -E "ERROR|WARN"
```

Then exercise it in the chat: an ordinary question, `/stat`, `/invite status`,
a photo, and `/reset` followed by a follow-up. Nothing about the phase should
be visible from a chat window — that is the success condition.

- [ ] **Step 7: Commit anything the deploy corrected, then finish the branch**

REQUIRED SUB-SKILL: superpowers:finishing-a-development-branch

---

## What this phase deliberately does not do

- **No axum, no HTTP, no second container.** That is 2b-2b, and it starts from
  a tree where the compiler already knows which side everything is on.
- **`Config` is not divided.** The adapter reads `TELEGRAM_BOT_TOKEN` itself,
  which is the first piece; the rest divides when core gets its own process
  and its own environment.
- **`return_url` still comes from `getMe`.** It becomes a `Core::start`
  argument here, which is honest about the dependency without solving it. It
  needs solving before core moves out.
- **No delta events.** The protocol keeps cumulative text until there is a
  socket to justify changing it.
