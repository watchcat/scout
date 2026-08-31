# Core Keyed on Accounts — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every `scout-core` entry point take an account id, so something that is not Telegram can drive the agent.

**Architecture:** The Telegram adapter resolves a Telegram id to an account once per update and passes account ids downward. `chat_id` — one number doing two unrelated jobs — splits into a `ReplyTo { channel, address }` carried for the run and a `conversation_id` used as the shown-flights key. A `TelegramId` newtype makes the remaining Telegram-shaped surface impossible to confuse with an account id.

**Tech Stack:** Rust workspace (`scout-api`, `scout-core`, `scout-telegram`, `scout-web`), DuckDB via the bundled `duckdb` crate, `rig` for the agent, `teloxide` for Telegram.

---

## Read this first

**This is a refactor. It must change no behaviour.**

The acceptance criterion is precise, because the obvious phrasing is wrong.
Changing a function signature *forces* mechanical edits to any test that calls
it, and roughly ten test sites are affected. So the rule is not "no test file
changes". It is:

> **No test's assertions or expected values change.** Only identifiers and
> struct-literal field names change, and only where a signature changed.

That is checkable, and you should check it: at the end, `git diff main -- '*/tests*'`
plus the in-file `mod tests` blocks should show renames and nothing else. A
changed `assert_eq!` right-hand side is a finding — stop and report it rather
than adjusting the expectation.

**The known-forced test edits, enumerated up front** so an unexpected one is
visible as unexpected:

| file | sites | edit |
|---|---|---|
| `crates/scout-core/src/tools/duffel.rs` | 2897, 2971, 3537, 3554, 3645 | `chat_id: 1` → `conversation_id: 1` |
| `crates/scout-core/src/tools/ignav.rs` | 715, 759, 858 | `chat_id: 7` / `chat_id: 1` → `conversation_id:` |
| `crates/scout-core/src/tools/reminders.rs` | 227, 249 | `chat_id: 99` → `reply_to: ReplyTo::telegram(99)` |
| `crates/scout-core/src/tools/shown.rs` | 244, 247 | `MAX_PER_CHAT` → `MAX_PER_CONVERSATION` |

Anything beyond this list is a finding.

## Verification commands

The toolchain on this machine is not pinned and both `cargo` and `cargo-clippy`
have resolved to mismatched versions in the past, producing `E0514` on
unrelated crates that reads like a broken branch. Check first:

```bash
cargo-clippy --version   # must match:
rustc --version
```

If they disagree, put the matching one first:

```bash
PATH="/run/current-system/sw/bin:$PATH" cargo clippy --workspace --all-targets
```

Throughout this plan:

```bash
cargo test --workspace    # 559 passing, 3 ignored, before and after
PATH="/run/current-system/sw/bin:$PATH" cargo clippy --workspace --all-targets
```

**Never run `cargo fmt`.** This repository is deliberately not rustfmt-formatted.

## File structure

| file | change |
|---|---|
| `crates/scout-api/src/lib.rs` | **add** `ReplyTo` |
| `crates/scout-core/src/ids.rs` | **create** — `TelegramId` |
| `crates/scout-core/src/lib.rs` | **add** `pub mod ids;` |
| `crates/scout-core/src/tools/shown.rs` | rename `chat_id` → `conversation_id` throughout |
| `crates/scout-core/src/tools/duffel.rs` | rename the tools' `chat_id` field |
| `crates/scout-core/src/tools/ignav.rs` | rename the tools' `chat_id` field |
| `crates/scout-core/src/tools/reminders.rs` | `chat_id: i64` → `reply_to: ReplyTo` |
| `crates/scout-core/src/agent.rs` | `build_agent` takes `ReplyTo` and `conversation_id` |
| `crates/scout-core/src/run.rs` | `run_agent` takes `account_id` and `ReplyTo` |
| `crates/scout-core/src/session.rs` | account-keyed; `conversation_scope` removed |
| `crates/scout-core/src/core.rs` | `log_request` account-keyed; `is_founder`/`is_admin` take `TelegramId` |
| `crates/scout-telegram/src/scope.rs` | **create** — `conversation_scope`, moved |
| `crates/scout-telegram/src/bot.rs` | resolve the account once per update |

---

### Task 1: `ReplyTo` in `scout-api`

Where an answer's side effects should be delivered. It lives beside
`DueDelivery`, which already carries the same `channel`/`address` pair.

**Files:**
- Modify: `crates/scout-api/src/lib.rs`

- [ ] **Step 1: Write the failing test**

`crates/scout-api/src/lib.rs` already has a `mod tests` at line 50 with three
tests in it, and `serde_json` is already a dev-dependency. Add this test inside
that existing module — do not create a second one:

```rust
    #[test]
    fn a_reply_to_survives_a_round_trip_and_names_its_channel() {
        // W4 puts this on a wire, so it has to serialise; the Telegram
        // constructor exists so an adapter cannot spell "telegram" wrong.
        let r = ReplyTo::telegram(-100123);
        assert_eq!(r.channel, "telegram");
        assert_eq!(r.address, "-100123");

        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<ReplyTo>(&json).unwrap(), r);
    }
```

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p scout-api
```

Expected: `cannot find type ReplyTo in this scope`.

- [ ] **Step 3: Add the type**

Add to `crates/scout-api/src/lib.rs`, after `DueDelivery`:

```rust
/// Where the side effects of a run should be delivered.
///
/// A run produces more than an answer: a reminder created mid-conversation
/// has to be sent somewhere later. That destination is a property of *where
/// the request arrived*, not of who made it — in a group chat the address is
/// the group, so a reminder asked for there goes back there. Resolving it
/// from the account's `deliveries` row instead would send it wherever that
/// account last spoke: `note_chat` records incoming chats, group or private
/// alike, last write wins. The destination would then depend on unrelated
/// later activity — quietly, and long after the reminder was made.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReplyTo {
    pub channel: String,
    pub address: String,
}

impl ReplyTo {
    /// A Telegram chat, by id. The channel string is written once here so
    /// no caller has to spell it.
    pub fn telegram(chat_id: i64) -> Self {
        Self { channel: "telegram".to_string(), address: chat_id.to_string() }
    }
}
```

- [ ] **Step 4: Run it and watch it pass**

```bash
cargo test -p scout-api
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-api
git commit -m "feat: a type for where a run's side effects should go"
```

---

### Task 2: `TelegramId` in `scout-core`

**Files:**
- Create: `crates/scout-core/src/ids.rs`
- Modify: `crates/scout-core/src/lib.rs`

- [ ] **Step 1: Create the type**

`crates/scout-core/src/ids.rs`:

```rust
//! Ids that are all `i64` and must not be confused.
//!
//! Core is keyed on account ids. A Telegram id is a different number in the
//! same type, and passing one where the other belongs is the mistake that
//! made `/stat` report an empty week. Wrapping the rarer of the two makes
//! that direction a compile error.
//!
//! Account ids stay bare `i64`. Wrapping them too would touch 68 signatures
//! and 595 references and force a `ToSql` impl for every DuckDB bind, for no
//! extra safety: the harmful direction is a Telegram id reaching something
//! that expects an account, and this already blocks it.

/// A Telegram user or chat id, as Telegram issues them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TelegramId(pub i64);

impl std::fmt::Display for TelegramId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
```

Add to `crates/scout-core/src/lib.rs`, beside the other `pub mod` lines:

```rust
pub mod ids;
```

- [ ] **Step 2: Prove the type actually blocks the mistake**

This is a compile-time guarantee, so a runtime test cannot demonstrate it.
Demonstrate it the way this repository demonstrates any other guarantee —
break it and watch it fail. Temporarily add to `crates/scout-core/src/ids.rs`:

```rust
#[allow(dead_code)]
fn demonstration(account_id: i64) -> TelegramId {
    account_id
}
```

```bash
cargo check -p scout-core
```

Expected: `error[E0308]: mismatched types — expected TelegramId, found i64`.

**Then delete those four lines.** This is a mutation check, not a change.

- [ ] **Step 3: Commit**

```bash
git add crates/scout-core/src/ids.rs crates/scout-core/src/lib.rs
git commit -m "feat: a Telegram id is not an account id, and now cannot be one"
```

---

### Task 3: `ShownFlights` is keyed by conversation

Pure rename. The key is an opaque `i64`, so no logic changes — but leaving the
parameter called `chat_id` while it receives a conversation id would be a
comment that lies, which this codebase treats as a defect.

**Files:**
- Modify: `crates/scout-core/src/tools/shown.rs`

- [ ] **Step 1: Rename throughout**

In `crates/scout-core/src/tools/shown.rs`, rename:

- the struct field `by_chat` → `by_conversation`
- the constant `MAX_PER_CHAT` → `MAX_PER_CONVERSATION`
- every parameter `chat_id: i64` → `conversation_id: i64` in `remember`,
  `find`, `offer_ids`, `remembered`, `recent_ids`

Update the doc comments that say "chat" to say "conversation". Change the
module-level and `MAX_PER_CONVERSATION` doc to read:

```rust
/// Most flights kept per conversation.
///
/// A trip conversation searches a dozen routes at seven rows each, and all
/// of them have to stay bindable — this holds that comfortably while still
/// bounding a conversation that never stops asking.
const MAX_PER_CONVERSATION: usize = 200;
```

And on the struct:

```rust
/// Every flight shown in each conversation recently, oldest first.
#[derive(Default)]
pub struct ShownFlights {
    by_conversation: DashMap<i64, Vec<Seen>>,
}
```

- [ ] **Step 2: Update the two forced test references**

In the same file's `mod tests`, `a_chat_that_never_stops_searching_cannot_grow_without_bound`
references `MAX_PER_CHAT` at lines 244 and 247 and again at 249 and 257.
Rename those to `MAX_PER_CONVERSATION`. **Change no assertion and no
expected value.**

- [ ] **Step 3: Run the tests**

```bash
cargo test -p scout-core shown
```

Expected: all `shown::tests` pass, unchanged in count.

- [ ] **Step 4: Commit**

```bash
git add crates/scout-core/src/tools/shown.rs
git commit -m "refactor: shown flights are remembered per conversation, not per chat"
```

---

### Task 4: The flight tools take a conversation id

Also a pure rename: these tools only pass the value through to `ShownFlights`.

**Files:**
- Modify: `crates/scout-core/src/tools/duffel.rs`
- Modify: `crates/scout-core/src/tools/ignav.rs`

- [ ] **Step 1: Rename the fields and their uses**

In both files, rename the struct field `pub chat_id: i64` to
`pub conversation_id: i64` on every tool that has one, and update each use:

`crates/scout-core/src/tools/duffel.rs:141`:

```rust
        self.shown.remember(self.conversation_id, out.picks.all(), now);
```

`crates/scout-core/src/tools/ignav.rs:525-529`:

```rust
        let Some(quoted) = self.shown.find(self.conversation_id, &args.ignav_id, at) else {
            let known = self.shown.offer_ids(self.conversation_id, at);
            tracing::info!(
                conversation_id = self.conversation_id,
```

- [ ] **Step 2: Update the eight forced test constructions**

`duffel.rs` lines 2897, 2971, 3537, 3554, 3645 and `ignav.rs` lines 715, 759,
858: change `chat_id:` to `conversation_id:`. **Keep every value as it is** —
`1` stays `1`, `7` stays `7`. Changing a value here would be changing a test.

- [ ] **Step 3: Run the tests**

```bash
cargo test -p scout-core
```

Expected: 445 passing, 3 ignored — the same as before this task.

- [ ] **Step 4: Commit**

```bash
git add crates/scout-core/src/tools/duffel.rs crates/scout-core/src/tools/ignav.rs
git commit -m "refactor: flight tools name their key a conversation"
```

---

### Task 5: `CreateReminderTool` takes a `ReplyTo`

The one task in this plan that adds a test, because it protects the property
that rules out the simpler design.

**Files:**
- Modify: `crates/scout-core/src/tools/reminders.rs`

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/scout-core/src/tools/reminders.rs`:

```rust
    #[tokio::test]
    async fn a_reminder_made_in_a_group_is_addressed_to_the_group() {
        // Why `ReplyTo` is carried for the run rather than resolved from
        // the account's `deliveries` row at delivery time: in a group,
        // the address is the group. Resolving per-account would send this
        // to the creator's private chat instead, silently.
        let (store, _d) = setup();
        let group = ReplyTo::telegram(-100_123);
        let create = CreateReminderTool {
            store: store.clone(),
            account_id: 1,
            reply_to: group.clone(),
        };

        let r = create
            .call(CreateReminderArgs {
                item: "team coffee".into(),
                interval_days: 30,
                next_due: Some("2026-09-01".into()),
            })
            .await
            .unwrap();

        assert_eq!(r.channel, "telegram");
        assert_eq!(r.address, "-100123", "a group reminder lost its group");
    }
```

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p scout-core reminders
```

Expected: FAIL — `struct CreateReminderTool has no field named reply_to`.

- [ ] **Step 3: Change the field**

In `crates/scout-core/src/tools/reminders.rs`:

```rust
pub struct CreateReminderTool {
    pub store: Store,
    pub account_id: i64,
    /// Where a reminder made in this run should be delivered. Carried
    /// rather than looked up, because in a group the address is the group.
    pub reply_to: scout_api::ReplyTo,
}
```

And in `call`, replace the `chat_id` destructuring and the `create_reminder`
arguments:

```rust
        let store = self.store.clone();
        let account_id = self.account_id;
        let (channel, address) = (self.reply_to.channel.clone(), self.reply_to.address.clone());
        tokio::task::spawn_blocking(move || {
            store.create_reminder(
                account_id,
                &channel,
                &address,
                &args.item,
                args.interval_days,
                &next_due,
            )
        })
        .await
```

Add the import at the top of the file if it is not already there:

```rust
use scout_api::ReplyTo;
```

- [ ] **Step 4: Update the two forced test constructions**

Lines 227 and 249: `chat_id: 99` → `reply_to: ReplyTo::telegram(99)`. The
existing assertion `assert_eq!(r.address, "99")` must keep passing untouched —
that is the proof the change is behaviour-preserving.

- [ ] **Step 5: Run the tests**

```bash
cargo test -p scout-core reminders
```

Expected: PASS, one more test than before.

- [ ] **Step 6: Mutation-check the new test**

Temporarily change `&address` to `"0"` in `create_reminder`. Run:

```bash
cargo test -p scout-core a_reminder_made_in_a_group
```

Expected: FAIL with "a group reminder lost its group". **Revert it.**

- [ ] **Step 7: Commit**

```bash
git add crates/scout-core/src/tools/reminders.rs
git commit -m "refactor: a reminder is addressed by where it was asked for"
```

---

### Task 6: `build_agent` takes a `ReplyTo` and a conversation id

**Files:**
- Modify: `crates/scout-core/src/agent.rs`

- [ ] **Step 1: Change the signature**

`crates/scout-core/src/agent.rs`, at `build_agent`:

```rust
/// Built per incoming message: tools capture the requesting account's
/// identity, so the LLM never sees or chooses ids.
pub fn build_agent(
    d: &AgentDeps,
    account_id: i64,
    reply_to: &scout_api::ReplyTo,
    conversation_id: i64,
    facts: &[(String, String)],
) -> rig::agent::Agent<openai::completion::CompletionModel> {
```

- [ ] **Step 2: Thread the two values to their tools**

Replace the `chat_id` uses inside `build_agent`. The reminder tool takes the
destination:

```rust
        .tool(CreateReminderTool {
            store: d.store.clone(),
            account_id,
            reply_to: reply_to.clone(),
        })
```

The three flight-related tools take the conversation — `AddTripOptionTool`,
`FlightSearchTool` and `BookingLinksTool` each had `chat_id,` and now take
`conversation_id,`.

- [ ] **Step 3: Compile**

```bash
cargo check -p scout-core
```

Expected: one error, in `run.rs`, which Task 7 fixes. If there are errors
anywhere else, a `chat_id` use was missed.

- [ ] **Step 4: Commit (with Task 7, since core does not compile alone)**

Do not commit yet. Continue to Task 7.

---

### Task 7: `run_agent` takes an account id and a `ReplyTo`

**Files:**
- Modify: `crates/scout-core/src/run.rs`

- [ ] **Step 1: Change the signature and drop the internal conversion**

`crates/scout-core/src/run.rs`:

```rust
pub async fn run_agent(
    core: &Core,
    events: scout_api::EventSink,
    account_id: i64,
    reply_to: &scout_api::ReplyTo,
    conversation_id: i64,
    prompt: &str,
) -> anyhow::Result<String> {
    let facts = {
        let store = core.deps.store.clone();
        tokio::task::spawn_blocking(move || store.list_facts(account_id)).await??
    };
    let agent = build_agent(&core.deps, account_id, reply_to, conversation_id, &facts);
```

The line `let account_id = crate::session::account_of(core, user_id).await?;`
is deleted: the caller now supplies it.

- [ ] **Step 2: Compile**

```bash
cargo check -p scout-core
```

Expected: clean. `scout-telegram` will not compile until Task 11.

- [ ] **Step 3: Commit Tasks 6 and 7 together**

```bash
git add crates/scout-core/src/agent.rs crates/scout-core/src/run.rs
git commit -m "refactor: a run is given an account and a destination, not a chat"
```

---

### Task 8: `resolve_conversation` and `reset` take an account id

**Files:**
- Modify: `crates/scout-core/src/session.rs`

- [ ] **Step 1: Change both signatures**

In `crates/scout-core/src/session.rs`:

```rust
pub async fn resolve_conversation(
    core: &Core,
    account_id: i64,
    scope: &str,
    text: &str,
) -> anyhow::Result<i64> {
    let ttl = SESSION_TTL.as_secs() as i64;
```

Delete the line `let account_id = account_of(core, user_id).await?;`.

Then replace the three `tracing` calls in that function, which log `user_id`,
with `account_id` — the value they name must be the value they have:

```rust
            tracing::info!(account_id, id, "session expired but topic continues; keeping context");
```
```rust
            tracing::info!(account_id, "session expired; starting fresh");
```
```rust
            tracing::warn!(error = %e, account_id, "continuation check failed; starting fresh");
```

And `reset`:

```rust
pub async fn reset(core: &Core, account_id: i64, scope: &str) -> anyhow::Result<i64> {
    let store = core.store();
    let scope = scope.to_string();
    crate::core::blocking(move || store.start_conversation(account_id, &scope)).await
}
```

- [ ] **Step 2: Compile**

```bash
cargo check -p scout-core
```

Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add crates/scout-core/src/session.rs
git commit -m "refactor: a conversation is resolved for an account"
```

---

### Task 9: `over_daily_cap` takes an account id

**Files:**
- Modify: `crates/scout-core/src/session.rs`
- Modify: `crates/scout-core/src/core.rs`

- [ ] **Step 1: Add the account-level founder question**

In `crates/scout-core/src/core.rs`, beside `is_founder`:

```rust
    /// Whether this account belongs to a founder.
    ///
    /// A founder is a Telegram id by configuration — `ALLOWED_TELEGRAM_USER_IDS`
    /// — so the question can only be asked of an account by looking up which
    /// Telegram ids it can prove. That is one query, and it buys the right
    /// answer for a founder who signs in from a browser instead.
    pub async fn founder_account(&self, account_id: i64) -> anyhow::Result<bool> {
        let store = self.store();
        let ids = blocking(move || store.telegram_ids(account_id)).await?;
        Ok(ids.iter().any(|id| self.cfg.allowed_user_ids.contains(id)))
    }
```

- [ ] **Step 2: Change `over_daily_cap`**

In `crates/scout-core/src/session.rs`:

```rust
pub async fn over_daily_cap(core: &Core, account_id: i64) -> Option<String> {
    match core.founder_account(account_id).await {
        Ok(true) => return None,
        Ok(false) => {}
        // Same reasoning as a failed count below: the cap is a cost guard,
        // not access control, and a database blip must not silence everyone.
        Err(e) => {
            tracing::warn!(error = %e, account_id, "founder check failed; letting it through");
            return None;
        }
    }
    let cap = core.cfg.invite_daily_requests;
    let store = core.deps.store.clone();
    let used = match crate::core::blocking(move || store.requests_today(account_id)).await {
        Ok(used) => used,
        Err(e) => {
            tracing::warn!(error = %e, account_id, "daily cap check failed; letting it through");
            return None;
        }
    };
    (used >= cap).then(|| {
        tracing::info!(account_id, used, cap, "daily cap reached");
        format!("You've used today's {cap} requests. It resets at midnight UTC.")
    })
}
```

- [ ] **Step 3: Compile and test**

```bash
cargo check -p scout-core && cargo test -p scout-core
```

Expected: clean, and the same test count as after Task 5.

- [ ] **Step 4: Commit**

```bash
git add crates/scout-core/src/session.rs crates/scout-core/src/core.rs
git commit -m "refactor: the daily cap is asked about an account"
```

---

### Task 10: `Core::log_request` takes an account id

Missed by the design document; found while planning. The daily cap counts the
rows this writes, so a web caller that cannot log a request cannot be capped.

**Files:**
- Modify: `crates/scout-core/src/core.rs`

- [ ] **Step 1: Change the signature**

In `crates/scout-core/src/core.rs`:

```rust
    /// Records a request against an account.
    pub async fn log_request(&self, account_id: i64, kind: &'static str) -> anyhow::Result<()> {
        let store = self.store();
        blocking(move || store.log_request(account_id, kind)).await
    }
```

`note_display_name` and `note_address` keep taking Telegram ids: both record
facts that only Telegram supplies, and neither is on the path a web caller
takes.

- [ ] **Step 2: Compile**

```bash
cargo check -p scout-core
```

Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add crates/scout-core/src/core.rs
git commit -m "refactor: a request is logged against an account"
```

---

### Task 11: The adapter converts at its edge

**Files:**
- Create: `crates/scout-telegram/src/scope.rs`
- Modify: `crates/scout-telegram/src/main.rs`
- Modify: `crates/scout-telegram/src/bot.rs`
- Modify: `crates/scout-core/src/session.rs` (remove `conversation_scope`)

- [ ] **Step 1: Move `conversation_scope` to the adapter, with its test**

Create `crates/scout-telegram/src/scope.rs`:

```rust
//! Which conversation a Telegram update belongs to.
//!
//! The rule is Telegram-shaped — it reads a chat id against a user id — so it
//! lives here. The strings it produces are core's vocabulary: `direct` is the
//! thread the web client shares, which is why the scope column has said so
//! since phase one.

/// In a 1:1 chat Telegram makes the chat id equal the user id, and that is
/// the thread the web app will share. Anywhere else is a room with other
/// people in it and keeps its own history.
pub fn conversation_scope(chat_id: i64, user_id: i64) -> String {
    if chat_id == user_id {
        "direct".to_string()
    } else {
        format!("telegram:{chat_id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scope_of_a_private_chat_is_shared_and_a_group_is_not() {
        // Telegram makes chat id equal user id in a 1:1 chat, and that
        // thread is the one the web app will share. A group is a room with
        // other people in it and keeps its own history.
        assert_eq!(conversation_scope(4242, 4242), "direct");
        assert_eq!(conversation_scope(-100123, 4242), "telegram:-100123");
    }
}
```

Delete `conversation_scope` and its test
`the_scope_of_a_private_chat_is_shared_and_a_group_is_not` from
`crates/scout-core/src/session.rs`. This is a move, not a deletion: the test
travels with the function unchanged, so the workspace total stays 560.

Add to `crates/scout-telegram/src/main.rs`:

```rust
mod scope;
```

- [ ] **Step 2: Give `account_of` the newtype**

After Task 8 nothing in core calls it — the adapter is its only caller — so
this is the moment its signature can say what it takes. In
`crates/scout-core/src/session.rs`:

```rust
/// The account behind a Telegram user, created on first sight.
///
/// The one place the two id spaces meet. Everything below is keyed by
/// account id, and the argument type is now what stops a caller passing the
/// wrong number: it used to be an `i64` with a hopeful name.
pub async fn account_of(core: &Core, id: crate::ids::TelegramId) -> anyhow::Result<i64> {
    let store = core.deps.store.clone();
    crate::core::blocking(move || store.account_for_telegram(id.0)).await
}
```

- [ ] **Step 3: Convert once per update in `bot.rs`**

At each of the three sites, resolve the account before calling core.

`/reset`, at `crates/scout-telegram/src/bot.rs:419`:

```rust
            let scope = crate::scope::conversation_scope(msg.chat.id.0, user_id);
            let account_id = match scout_core::session::account_of(
                &app.core,
                scout_core::ids::TelegramId(user_id),
            )
            .await
            {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, "could not resolve an account for /reset");
                    return;
                }
            };
            if let Err(e) = scout_core::session::reset(&app.core, account_id, &scope).await {
```

The text handler, around `crates/scout-telegram/src/bot.rs:855-919`:

```rust
    let account_id = match scout_core::session::account_of(
        &app.core,
        scout_core::ids::TelegramId(user_id),
    )
    .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "could not resolve an account");
            return;
        }
    };
    if let Some(refusal) = scout_core::session::over_daily_cap(&app.core, account_id).await {
```

```rust
    let scope = crate::scope::conversation_scope(chat_id.0, user_id);
    let conversation_id = match scout_core::session::resolve_conversation(&app.core, account_id, &scope, &text).await {
```

```rust
        scout_core::run::run_agent(
            &app.core,
            events,
            account_id,
            &scout_api::ReplyTo::telegram(chat_id.0),
            conversation_id,
            &prompt,
        ),
```

Apply the identical shape to the photo handler at lines 979–1104.

- [ ] **Step 4: Update `log_request` at its adapter helper**

`crates/scout-telegram/src/bot.rs:564` currently takes a `user_id` and hands
it to core. It now needs an account id, which its two callers already have:

```rust
fn log_request(app: &Arc<App>, account_id: i64, kind: &'static str) {
```

and inside, `core.log_request(account_id, kind)`. Its two call sites at lines
859 and 996 become `log_request(&app, account_id, "text")` and
`log_request(&app, account_id, "photo")`.

- [ ] **Step 5: Update `is_founder` and `is_admin` calls**

These now take `TelegramId`. At `bot.rs:312`, `431`, `451` and anywhere else
the compiler points, wrap the argument:

```rust
    match app.core.is_admin(scout_core::ids::TelegramId(user_id)) {
```

Change the two signatures in `crates/scout-core/src/core.rs`:

```rust
    pub fn is_founder(&self, id: crate::ids::TelegramId) -> bool {
        is_founder(&self.cfg.allowed_user_ids, id.0)
    }

    pub fn is_admin(&self, id: crate::ids::TelegramId) -> bool {
        is_admin_id(&self.cfg.admin_user_ids, id.0)
    }
```

The free functions `is_founder`/`is_admin_id` keep taking `i64`: they are
pure set lookups, their own tests call them directly, and changing them would
edit tests for no safety gained.

- [ ] **Step 6: Build and test the workspace**

```bash
cargo test --workspace
```

Expected: 560 passing, 3 ignored — 559 before, plus the group-reminder test.

- [ ] **Step 7: Commit**

```bash
git add crates/scout-telegram crates/scout-core/src/session.rs crates/scout-core/src/core.rs
git commit -m "refactor: Telegram converts to an account at its own edge"
```

---

### Task 12: Verification

- [ ] **Step 1: The whole suite and clippy**

```bash
cargo test --workspace
PATH="/run/current-system/sw/bin:$PATH" cargo clippy --workspace --all-targets
```

Expected: 560 passing, 3 ignored; clippy silent apart from the pre-existing
`proc-macro-error2` future-incompat note from a transitive dependency.

- [ ] **Step 2: Audit every test-file change**

```bash
git diff main --stat -- crates
```

Then read every hunk inside a `mod tests` block. Each must be one of: a field
name, an identifier, or the moved `conversation_scope` test. **Any changed
assertion or expected value is a finding — report it, do not adjust it.**

- [ ] **Step 3: Mutation-check the two properties this refactor could break**

`ReplyTo` reaching the wrong destination:

```bash
# In build_agent, pass `&scout_api::ReplyTo::telegram(0)` to CreateReminderTool
cargo test -p scout-core a_reminder_made_in_a_group
```

Expected: FAIL. Revert.

The shown-flights key losing its per-conversation isolation:

```bash
# In build_agent, pass `account_id` instead of `conversation_id` to the three flight tools
cargo test -p scout-core an_id_from_another_chat_is_not_answerable
```

Expected: this test passes either way, because it calls `ShownFlights`
directly. That is the point of running it: it proves the existing suite does
**not** cover the wiring, so add the missing coverage rather than assuming it.
Revert the mutation, then add to `crates/scout-core/src/tools/shown.rs`:

```rust
    #[test]
    fn two_conversations_of_one_account_do_not_share_what_was_shown() {
        // The wiring this protects: the flight tools are handed a
        // conversation id, not an account id. Keyed by account, a group
        // chat and a private chat would renumber each other's options.
        let shown = ShownFlights::default();
        let now = Instant::now();
        shown.remember(11, vec![flight("private", 100.0)], now);
        shown.remember(22, vec![flight("group", 200.0)], now);
        assert!(shown.find(11, "group", now).is_none());
        assert!(shown.find(22, "private", now).is_none());
    }
```

- [ ] **Step 4: Final commit**

```bash
cargo test --workspace   # 561 passing, 3 ignored
git add crates
git commit -m "test: two conversations do not share what each was shown"
```

---

## What this deliberately does not do

- No SSE endpoint, no browser client, no web route of any kind. That is W3,
  and it is unblocked by this.
- No decision about what a browser passes as `ReplyTo`. A reminder created on
  the web by someone with no Telegram identity has nowhere to go, and that is
  a product question for W3.
- No `AccountId` newtype. Rejected on diff size in the design; revisit if the
  `i64` confusion ever recurs on the account side.
- `note_display_name`, `note_address`, `is_founder` and `is_admin` stay
  Telegram-shaped, because what they record or decide is Telegram-specific.
