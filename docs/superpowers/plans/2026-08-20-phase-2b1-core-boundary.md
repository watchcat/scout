# Phase 2b-1 — Move the Logic to Where It Belongs: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Shrink what the Telegram adapter asks of the rest of Scout from 28
store methods to about five, by moving the work that is not transport —
invites, moderation, `/stat`, `/advert`, session resolution, the daily cap —
out of `bot.rs` and into modules that know nothing about Telegram.

**Architecture:** A `Core` struct holds the configuration and dependencies
that answering a question needs; `App` keeps only what talking to Telegram
needs and borrows a `Core`. Command handlers become functions on `Core` that
take parsed arguments and return text. Anything that must *send* something —
`/advert`, `/invite announce` — splits in two: core decides who and what,
the adapter delivers, core records what happened. One binary throughout.

**Tech Stack:** Rust 1.96 / edition 2021. No new dependencies.

---

## Why this comes before the network

Measured on the current tree:

| | |
|---|---|
| Distinct `store.*` methods called from `bot.rs` | **28** |
| `bot.rs` total | **2740 lines** |
| Config fields `bot.rs` uses | **3** — `allowed_user_ids`, `admin_user_ids`, `invite_daily_requests`, all authorization |
| Largest functions in `bot.rs` | `run_agent` 191, `handle_command` 158, `announce_round` 91, `handle_invite` 84, `handle_kick` 50 |

Twenty-eight is not what a Telegram adapter needs from Scout. It is what
`bot.rs` needs because `bot.rs` is doing Scout's job as well as Telegram's.
Turning all 28 into HTTP endpoints would carry that mistake across a network,
where it is far more expensive to undo.

**The success criterion is a number.** After this plan, the adapter should
reach core through roughly five doors:

1. answer a message, receiving events
2. read the member set (the gate's cache)
3. take a command and get text back
4. collect what needs delivering, and report what happened
5. hand over an uploaded photo

Task 10 measures it. If the count has not collapsed, the split is not done.

## A note on moves

Most tasks here move existing, working, tested code. Reproducing 200-line
function bodies in a plan invites transcription errors and hides the parts
that genuinely change. So a move is specified as: **which function, from
where, to where, and exactly which lines of its signature or body differ.**
Code blocks appear where code is genuinely new or genuinely changed. If a
step shows no body, the body is unchanged — copy it verbatim.

## File structure

| File | Responsibility | Change |
|---|---|---|
| `src/core.rs` | `Core`: config + deps, the handle everything below hangs off | **create** |
| `src/session.rs` | which conversation a message belongs to, and whether it is allowed | **create** |
| `src/invites.rs` | rounds, claims, the waitlist, moderation, announcements | **create** |
| `src/run.rs` | `run_agent` and its wrap-up/repair tail | **create** (moved) |
| `src/bot.rs` | routing, rendering, chunking, sending, Telegram command parsing | shrinks by ~1100 lines |
| `src/stats.rs` | already Telegram-free; gains the `/stat` handler | modify |
| `src/main.rs` | builds a `Core`, then an `App` around it | modify |

`bot.rs` should end around 1600 lines and contain nothing that would need
rewriting to serve a web request.

---

## Task 1: A Core to hang things off

**Files:**
- Create: `src/core.rs`
- Modify: `src/bot.rs` (`App`), `src/main.rs`
- Test: `src/core.rs`

- [ ] **Step 1: Write the failing test**

Create `src/core.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_founder_is_exempt_from_the_daily_cap_and_a_member_is_not() {
        // Authorization is core's business: the adapter's gate is a cache in
        // front of this, never the decision itself.
        let founders: std::collections::HashSet<i64> = [11_i64].into_iter().collect();
        assert!(is_founder(&founders, 11));
        assert!(!is_founder(&founders, 22));
    }

    #[test]
    fn an_admin_is_named_by_telegram_id_because_that_is_what_env_holds() {
        let admins: std::collections::HashSet<i64> = [99_i64].into_iter().collect();
        assert!(is_admin_id(&admins, 99));
        assert!(!is_admin_id(&admins, 11));
    }
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test a_founder_is_exempt_from_the_daily_cap`
Expected: FAIL — `cannot find function 'is_founder'`

- [ ] **Step 3: Write the implementation**

Above the tests in `src/core.rs`:

```rust
use crate::agent::AgentDeps;
use crate::config::Config;
use std::collections::HashSet;

/// Everything answering a question needs, and nothing about how the question
/// arrived.
///
/// In phase 2b-2 this is what the HTTP service owns. Nothing here may learn
/// what a `ChatId` is.
pub struct Core {
    pub cfg: Config,
    pub deps: AgentDeps,
}

/// Founders are the people paying for the bot, named in the environment by
/// Telegram id because that is the only name they had when the list was
/// written. Exempt from the daily cap.
pub fn is_founder(founders: &HashSet<i64>, telegram_id: i64) -> bool {
    founders.contains(&telegram_id)
}

/// Admin rights are likewise granted by Telegram id in the environment.
pub fn is_admin_id(admins: &HashSet<i64>, telegram_id: i64) -> bool {
    admins.contains(&telegram_id)
}

impl Core {
    pub fn is_founder(&self, telegram_id: i64) -> bool {
        is_founder(&self.cfg.allowed_user_ids, telegram_id)
    }

    pub fn is_admin(&self, telegram_id: i64) -> bool {
        is_admin_id(&self.cfg.admin_user_ids, telegram_id)
    }

    pub fn store(&self) -> crate::store::Store {
        self.deps.store.clone()
    }
}
```

Add `mod core;` to `src/main.rs` in alphabetical position, after `mod config;`.

- [ ] **Step 4: Move `cfg` and `deps` out of `App`**

In `src/bot.rs`, replace the `cfg` and `deps` fields of `App` with a `core`:

```rust
pub struct App {
    pub core: Arc<crate::core::Core>,
```

Keep `chats`, `replies`, `streams` and `members` exactly as they are — every
one of them is a fact about Telegram.

Then let the compiler find every use: `app.cfg` becomes `app.core.cfg` and
`app.deps` becomes `app.core.deps`.

```bash
cargo build 2>&1 | grep -E '^error' | head -20
```

- [ ] **Step 5: Build the Core in main.rs**

In `src/main.rs`, replace the `App` construction:

```rust
    let core = Arc::new(core::Core { cfg, deps });

    let app = Arc::new(bot::App {
        core: core.clone(),
        chats: DashMap::new(),
        replies: DashMap::new(),
        streams: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        members,
    });
```

The startup log reads `cfg` before this point, so move the `tracing::info!`
call above the `Core` construction or read the values from `core.cfg`.

- [ ] **Step 6: Run the suite**

Run: `cargo test`
Expected: **432 passed** (430 + 2 new), 3 ignored.

- [ ] **Step 7: Commit**

```bash
git add src/core.rs src/bot.rs src/main.rs
git commit -m "refactor: a core that knows nothing about telegram"
```

---

## Task 2: Session resolution leaves the adapter

`conversation_scope`, `resolve_conversation`, `over_daily_cap` and
`account_of` decide *whether* a message is answered and *which thread* it
joins. None of that is transport.

**Files:**
- Create: `src/session.rs`
- Modify: `src/bot.rs` (remove the four functions; call the new ones)
- Test: `src/session.rs`

- [ ] **Step 1: Move the four functions verbatim**

Move these from `src/bot.rs` into a new `src/session.rs`, bodies unchanged
except as noted:

| Function | Signature change |
|---|---|
| `conversation_scope(chat_id, user_id) -> String` | none; make it `pub` |
| `account_of(app: &App, telegram_id) -> Result<i64>` | takes `core: &Core`; body uses `core.store()` |
| `resolve_conversation(app: &App, user_id, scope, text) -> Result<i64>` | takes `core: &Core`; `app.deps.llm` becomes `core.deps.llm` |
| `over_daily_cap(app: &Arc<App>, user_id) -> Option<String>` | takes `core: &Core`; `app.cfg.allowed_user_ids.contains(..)` becomes `core.is_founder(user_id)` |

Add `mod session;` to `src/main.rs`. `SESSION_TTL` moves too, as a
`pub const` in `session.rs`; `bot.rs` still needs it for
`take_expired_session` and refers to it as `crate::session::SESSION_TTL`.

Move the existing tests for `conversation_scope` with it.

- [ ] **Step 2: Add a test that the cap exempts founders**

In `src/session.rs`:

```rust
    #[test]
    fn the_scope_of_a_private_chat_is_shared_and_a_group_is_not() {
        assert_eq!(conversation_scope(4242, 4242), "direct");
        assert_eq!(conversation_scope(-100123, 4242), "telegram:-100123");
    }
```

(If the identical test moved from `bot.rs`, keep only one copy.)

- [ ] **Step 3: Build and fix call sites**

Run: `cargo build 2>&1 | grep -E '^error' | head -20`
Expected: errors in `handle_text`, `handle_photo`, `handle_reaction` where
the four functions were called. Prefix each with `crate::session::` and pass
`&app.core` instead of `&app`.

- [ ] **Step 4: Run the suite**

Run: `cargo test`
Expected: 432 passed, 3 ignored. No test should change meaning — this task
moves code, it does not alter a decision.

- [ ] **Step 5: Commit**

```bash
git add src/session.rs src/bot.rs src/main.rs
git commit -m "refactor: which conversation a message joins is not a telegram question"
```

---

## Task 3: Invites and moderation leave the adapter

**Files:**
- Create: `src/invites.rs`
- Modify: `src/bot.rs`
- Test: `src/invites.rs`

- [ ] **Step 1: Move the pure helpers first**

Move verbatim from `src/bot.rs` to a new `src/invites.rs`, making each
`pub(crate)`: `parse_invite`, `InviteCmd`, `check_round_name`,
`check_capacity`, `parse_user_id`, `status_report`, `new_round_reply`.

Move their tests too — these already have good coverage and it must not be
lost in transit.

Add `mod invites;` to `src/main.rs`.

- [ ] **Step 2: Run the suite to confirm the move was clean**

Run: `cargo test`
Expected: 432 passed. A drop in the count means tests were left behind.

- [ ] **Step 3: Move the decisions**

Move `handle_invite` and `handle_kick` into `src/invites.rs` and change them
from "handler that replies" to "function that returns text":

```rust
/// Runs an invite command and returns what to say about it.
///
/// Returns text rather than sending it: the web app will want the same
/// answer rendered differently, and core has no way to send anything.
pub async fn invite(core: &Core, admin_telegram_id: i64, cmd: InviteCmd) -> String
```

```rust
/// `kicking` true revokes, false restores.
pub async fn kick(core: &Core, admin_telegram_id: i64, target: &str, kicking: bool) -> String
```

Both begin with the admin check that is currently inside them:

```rust
    if !core.is_admin(admin_telegram_id) {
        return NOT_ADMIN.to_string();
    }
```

Move `NOT_ADMIN` into `invites.rs` as `pub(crate) const`.

The `InviteCmd::Announce` arm is *not* handled here — it needs to send
messages, and Task 4 splits it. For now have `invite()` return a placeholder
for that arm only:

```rust
        InviteCmd::Announce(_) => unreachable!("announce is handled by the adapter; see plan task 4"),
```

and leave `handle_command`'s existing `Announce` path calling the old
`announce_round` until Task 4 replaces it. This is the one place where the
plan deliberately leaves a seam open for one task.

- [ ] **Step 4: Point `handle_command` at them**

In `src/bot.rs`, the `Command::Invite`, `Command::Kick` and `Command::Unkick`
arms become:

```rust
        Command::Invite(arg) => {
            let Some(user_id) = sender_id(&msg) else { return Ok(()) };
            match parse_invite(&arg) {
                Ok(crate::invites::InviteCmd::Announce(code)) => {
                    announce_round(&bot, &app, msg.chat.id, &code).await?;
                }
                Ok(cmd) => {
                    let reply = crate::invites::invite(&app.core, user_id, cmd).await;
                    bot.send_message(msg.chat.id, reply).await?;
                }
                Err(e) => {
                    bot.send_message(msg.chat.id, e).await?;
                }
            }
        }
        Command::Kick(arg) => {
            let Some(user_id) = sender_id(&msg) else { return Ok(()) };
            let reply = crate::invites::kick(&app.core, user_id, &arg, true).await;
            bot.send_message(msg.chat.id, reply).await?;
        }
        Command::Unkick(arg) => {
            let Some(user_id) = sender_id(&msg) else { return Ok(()) };
            let reply = crate::invites::kick(&app.core, user_id, &arg, false).await;
            bot.send_message(msg.chat.id, reply).await?;
        }
```

Note `parse_invite` runs adapter-side: it turns a Telegram command string
into a request. Parsing the wire format is the adapter's job; deciding what
the request means is not.

- [ ] **Step 5: Run the suite**

Run: `cargo test`
Expected: 432 passed, 3 ignored.

- [ ] **Step 6: Commit**

```bash
git add src/invites.rs src/bot.rs src/main.rs
git commit -m "refactor: who is admitted is not a telegram question"
```

---

## Task 4: Announcements split in two

`announce_round` currently decides who to reach, sends to them, and records
the outcome, in one function that only Telegram can run. Core must decide and
record; the adapter must send.

**Files:**
- Modify: `src/invites.rs` (add the two halves), `src/bot.rs` (`announce_round`)
- Test: `src/invites.rs`

- [ ] **Step 1: Write the failing test**

In `src/invites.rs`:

```rust
    #[tokio::test]
    async fn an_announcement_refuses_a_round_that_cannot_take_anyone() {
        let (s, _d) = crate::store::tests::test_store();
        s.create_round("autumn", 1).unwrap();
        let a = s.account_for_telegram(11).unwrap();
        s.claim_seat(a, 11, "autumn").unwrap();

        // Full. Announcing it would invite people to a door that is shut.
        match plan_announcement(&s, "autumn").unwrap() {
            Announcement::Refused(reason) => assert!(reason.contains("full")),
            Announcement::Ready { .. } => panic!("a full round must not be announced"),
        }
    }

    #[tokio::test]
    async fn an_announcement_reaches_the_longest_waiting_first_and_records_only_what_landed() {
        let (s, _d) = crate::store::tests::test_store();
        s.create_round("autumn", 1).unwrap();
        let first = s.account_for_telegram(11).unwrap();
        let second = s.account_for_telegram(22).unwrap();
        s.claim_seat(first, 11, "autumn").unwrap();
        // Both turned away, in order.
        s.claim_seat(second, 22, "autumn").unwrap();
        let third = s.account_for_telegram(33).unwrap();
        s.claim_seat(third, 33, "autumn").unwrap();

        s.create_round("winter", 5).unwrap();
        let Announcement::Ready { targets, text } = plan_announcement(&s, "winter").unwrap() else {
            panic!("an open round with room should be announceable");
        };
        assert_eq!(targets.iter().map(|t| t.0).collect::<Vec<_>>(), vec![second, third],
            "oldest first, so a round smaller than the queue reaches those who waited longest");
        assert!(text.contains("/start winter"), "the command, because a link cannot reach them");
        assert!(!text.contains("t.me/"), "a link only works on an empty chat");

        // Only the one that landed is stamped; the other must be retried.
        record_announcement(&s, &[(second, Reached::Yes), (third, Reached::No)]).unwrap();
        let again = match plan_announcement(&s, "winter").unwrap() {
            Announcement::Ready { targets, .. } => targets.iter().map(|t| t.0).collect::<Vec<_>>(),
            Announcement::Refused(r) => panic!("{r}"),
        };
        assert_eq!(again, vec![third], "a delivery that failed is tried again");
    }
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test an_announcement_refuses_a_round`
Expected: FAIL — `cannot find function 'plan_announcement'`

- [ ] **Step 3: Implement the two halves**

In `src/invites.rs`:

```rust
/// What an announcement would do, decided without sending anything.
pub enum Announcement {
    /// Nobody should be invited, and why.
    Refused(String),
    /// Who to reach, and what to say. `targets` is (account, address).
    Ready { targets: Vec<(i64, i64)>, text: String },
}

/// Chooses who hears that a round is open, oldest first.
///
/// Decides and does not send, so that the same decision serves any channel.
/// A closed, unknown or full round is refused here rather than discovered
/// halfway through a broadcast.
pub fn plan_announcement(store: &Store, code: &str) -> anyhow::Result<Announcement> {
    // Body: the refusal checks and target selection currently at the top of
    // `announce_round` in bot.rs, unchanged, returning these variants rather
    // than sending. `announce_message(code)` supplies `text`.
}

/// Records what actually happened, one entry per recipient: `true` reached,
/// `false` did not.
///
/// A recipient that was reached is stamped so a re-run skips them; one that
/// was not is left alone so a re-run retries. A recipient the adapter reports
/// as permanently gone is dropped from the waitlist entirely.
pub fn record_announcement(store: &Store, outcomes: &[(i64, Reached)]) -> anyhow::Result<()> {
    for (account_id, reached) in outcomes {
        match reached {
            Reached::Yes => store.mark_invited(*account_id)?,
            Reached::No => {}
            // Blocked the bot or deleted the account. Chasing them forever
            // would make every future announcement slower for everyone else.
            Reached::Gone => store.forget_waitlist(*account_id)?,
        }
    }
    Ok(())
}
```

`announce_message` moves into `invites.rs` with them.

The "permanently gone" case keeps its existing behaviour: `bot.rs` already
distinguishes `Delivered::Gone` and calls `forget_waitlist`. Add a third
state rather than overloading the bool:

```rust
pub enum Reached { Yes, No, Gone }
```

and make `record_announcement` take `&[(i64, Reached)]`, calling
`mark_invited` on `Yes`, nothing on `No`, and `forget_waitlist` on `Gone` —
which is what the test above already expects.

- [ ] **Step 4: Reduce `announce_round` to delivery**

In `src/bot.rs`:

```rust
async fn announce_round(bot: &Bot, app: &Arc<App>, chat_id: ChatId, code: &str) -> ResponseResult<()> {
    let store = app.core.store();
    let code_owned = code.to_string();
    let planned = match blocking(move || crate::invites::plan_announcement(&store, &code_owned)).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "could not plan the announcement");
            bot.send_message(chat_id, CLAIM_FAILED).await?;
            return Ok(());
        }
    };
    let (targets, text) = match planned {
        crate::invites::Announcement::Refused(reason) => {
            bot.send_message(chat_id, reason).await?;
            return Ok(());
        }
        crate::invites::Announcement::Ready { targets, text } => (targets, text),
    };

    let sent = broadcast(bot, &targets, &text, Some(ParseMode::Html)).await;
    let outcomes: Vec<(i64, crate::invites::Reached)> = sent
        .iter()
        .map(|(account_id, delivered)| {
            let reached = match delivered {
                Delivered::Ok => crate::invites::Reached::Yes,
                Delivered::Gone => crate::invites::Reached::Gone,
                Delivered::Failed => crate::invites::Reached::No,
            };
            (*account_id, reached)
        })
        .collect();

    let store = app.core.store();
    if let Err(e) = blocking(move || crate::invites::record_announcement(&store, &outcomes)).await {
        tracing::error!(error = %e, "could not record the announcement");
    }

    let reached = sent.iter().filter(|(_, d)| *d == Delivered::Ok).count();
    bot.send_message(chat_id, format!("Announced to {reached} of {} waiting.", sent.len()))
        .await?;
    Ok(())
}
```

- [ ] **Step 5: Run the suite**

Run: `cargo test`
Expected: 434 passed (432 + 2), 3 ignored.

- [ ] **Step 6: Remove the placeholder from Task 3**

Delete the `unreachable!` arm now that `announce_round` calls
`plan_announcement`. `invite()` never sees `Announce`, so the arm should
return a refusal rather than panic if it somehow does:

```rust
        InviteCmd::Announce(_) => {
            "Announcing needs a channel to send on; that is the adapter's job.".to_string()
        }
```

- [ ] **Step 7: Commit**

```bash
git add src/invites.rs src/bot.rs
git commit -m "refactor: core decides who hears about a round, the adapter delivers"
```

---

## Task 5: /stat leaves the adapter

**Files:**
- Modify: `src/stats.rs` (add the handler), `src/bot.rs`
- Test: `src/stats.rs`

- [ ] **Step 1: Move the decision**

`src/stats.rs` already holds `format_stats` and `parse_days` and has no
Telegram in it. Add the handler that `handle_command`'s `Command::Stat` arm
currently inlines:

```rust
/// The `/stat` answer as text.
///
/// `everyone` is refused for a non-admin here rather than in the adapter,
/// because the web app must not be able to ask for it either.
pub async fn report(core: &Core, telegram_id: i64, arg: &str) -> String
```

The body is the existing arm, with `is_admin(&app, user_id)` replaced by
`core.is_admin(telegram_id)`, and the account lookup done through
`core.store().account_for_telegram(telegram_id)`.

- [ ] **Step 2: Point the command at it**

```rust
        Command::Stat(arg) => {
            let Some(user_id) = sender_id(&msg) else { return Ok(()) };
            let reply = crate::stats::report(&app.core, user_id, &arg).await;
            bot.send_message(msg.chat.id, reply).await?;
        }
```

- [ ] **Step 3: Run the suite**

Run: `cargo test`
Expected: 434 passed, 3 ignored.

- [ ] **Step 4: Commit**

```bash
git add src/stats.rs src/bot.rs
git commit -m "refactor: usage figures are not a telegram question"
```

---

## Task 6: /advert splits like the announcement

**Files:**
- Modify: `src/invites.rs`, `src/bot.rs`
- Test: `src/invites.rs`

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn an_advert_reaches_everyone_with_a_known_address() {
        let (s, _d) = crate::store::tests::test_store();
        let a = s.account_for_telegram(11).unwrap();
        let b = s.account_for_telegram(22).unwrap();
        s.note_delivery(a, "telegram", "555").unwrap();
        s.note_delivery(b, "telegram", "666").unwrap();
        // 33 has an account but has never spoken, so there is nowhere to send.
        s.account_for_telegram(33).unwrap();

        let targets = advert_targets(&s).unwrap();
        assert_eq!(targets, vec![(a, 555), (b, 666)]);
    }
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test an_advert_reaches_everyone_with_a_known_address`
Expected: FAIL — `cannot find function 'advert_targets'`

- [ ] **Step 3: Implement**

```rust
/// Everyone an announcement could reach on Telegram, as (account, chat).
///
/// A thin wrapper today, but it is the door `/advert` goes through, and in
/// 2b-2 it becomes an endpoint rather than a store call.
pub fn advert_targets(store: &Store) -> anyhow::Result<Vec<(i64, i64)>> {
    store.broadcast_targets()
}
```

- [ ] **Step 4: Point `/advert` at it**

In `src/bot.rs`, the `Command::Advert` arm keeps `check_advert` (validation of
a Telegram message body) and its admin check moves to `core.is_admin`, then
calls `crate::invites::advert_targets(&store)` instead of
`store.broadcast_targets()`.

- [ ] **Step 5: Run the suite**

Run: `cargo test`
Expected: 435 passed, 3 ignored.

- [ ] **Step 6: Commit**

```bash
git add src/invites.rs src/bot.rs
git commit -m "refactor: an advert asks core who to reach"
```

---

## Task 7: The agent run leaves the adapter

After phase 2a `run_agent` already emits events rather than drawing them, so
this is a move rather than a change.

**Files:**
- Create: `src/run.rs`
- Modify: `src/bot.rs`

- [ ] **Step 1: Move it**

Move into `src/run.rs`, verbatim except the signature's first parameter:

- `run_agent` — `app: &App` becomes `core: &Core`
- `agent_error_message`
- `tail_chars`, `RUN_BUDGET`, `STREAM_STALL`, `WRAP_UP_BUDGET`, `WRAP_UP_CONTEXT`
- `trim_history`, `is_text_only`, `is_plain_user_text`, `last_messages_text`
- `save_history`, `load_history`

Inside the body, `app.deps` becomes `core.deps` and `app.cfg` becomes
`core.cfg`. `blocking` is used by both files — move it to `src/core.rs` as
`pub(crate) async fn blocking` and have both refer to `crate::core::blocking`.

Move the tests for `trim_history`, `load_history`, `save_history` and
`last_messages_text` with them.

Add `mod run;` to `src/main.rs`.

- [ ] **Step 2: Build and fix the two call sites**

`handle_text` and `handle_reaction` call `run_agent(&app, events, ..)`; they
become `crate::run::run_agent(&app.core, events, ..)`.

Run: `cargo build 2>&1 | grep -E '^error' | head -20`

- [ ] **Step 3: Run the suite**

Run: `cargo test`
Expected: 435 passed, 3 ignored. `run_agent` is the largest thing moving; a
changed test count means tests were left behind in `bot.rs`.

- [ ] **Step 4: Commit**

```bash
git add src/run.rs src/bot.rs src/core.rs src/main.rs
git commit -m "refactor: answering a question is not a telegram question"
```

---

## Task 8: Photos hand over bytes

`describe_photo` already lives in `vision.rs` and knows nothing about
Telegram; `handle_photo` downloads the file and calls it. That division is
already right. What is not right is that `handle_photo` also writes the
draft and decides the prompt.

**Files:**
- Modify: `src/bot.rs`, `src/run.rs`

- [ ] **Step 1: Move the prompt construction**

Move the photo prompt assembly out of `handle_photo` into `src/run.rs`:

```rust
/// Turns a described photo into the prompt the agent answers.
///
/// Core's business: the web app will upload photos too, and it must get the
/// same behaviour without copying this string.
pub fn photo_prompt(description: &str, caption: Option<&str>) -> String
```

The body is the existing `format!` in `handle_photo`, unchanged.

- [ ] **Step 2: Point `handle_photo` at it**

`handle_photo` keeps: `GetFile`, `download_file`, and the draft in
`app.chats` (a draft is a Telegram-chat concept until phase three). It calls
`crate::run::photo_prompt(..)` for the text.

- [ ] **Step 3: Run the suite**

Run: `cargo test`
Expected: 435 passed, 3 ignored.

- [ ] **Step 4: Commit**

```bash
git add src/bot.rs src/run.rs
git commit -m "refactor: what a photo asks is decided in core"
```

---

## Task 9: The gate reads its cache and nothing else

**Files:**
- Modify: `src/bot.rs`

- [ ] **Step 1: Confirm the gate touches no store**

```bash
grep -n 'fn is_member\|fn is_member_id\|fn is_allowed' src/bot.rs
```

`is_member_id` should read only `app.core.cfg.allowed_user_ids` and
`app.members`. If it reaches the store, that is the hot path doing a disk
read per stranger and must be fixed here.

- [ ] **Step 2: Add the assertion as a test**

```rust
    #[test]
    fn the_gate_answers_from_memory_alone() {
        // Not a style point: the gate runs on every update, including from
        // people who were never invited. A store read here means anyone who
        // finds the bot can make it do disk work by typing at it.
        let src = include_str!("bot.rs");
        let start = src.find("fn is_member_id").expect("gate must exist");
        let end = src[start..].find("\n}").expect("gate must end") + start;
        let body = &src[start..end];
        assert!(!body.contains("store"), "the gate must not touch the store");
        assert!(!body.contains(".await"), "the gate must not await anything");
    }
```

- [ ] **Step 3: Run it**

Run: `cargo test the_gate_answers_from_memory_alone`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/bot.rs
git commit -m "test: the gate stays a memory read"
```

---

## Task 10: Measure the surface

The point of the whole plan. If this does not show a collapse, something was
moved to the wrong side.

**Files:** none — this is measurement.

- [ ] **Step 1: Count what the adapter still asks of the store**

```bash
grep -oE 'store\.[a-z_]+\(' src/bot.rs | sort -u
grep -oE 'store\.[a-z_]+\(' src/bot.rs | sort -u | wc -l
```

Expected: down from 28 to roughly 5. The survivors should be only:
`account_for_telegram` (resolving the sender), `active_members` (the gate
cache), `note_delivery` (recording where someone spoke), `log_request`, and
`remember_user`. Anything else on that list is logic still sitting in the
adapter — name it in the commit message rather than quietly accepting it.

- [ ] **Step 2: Check the shape of what is left**

```bash
wc -l src/bot.rs src/core.rs src/session.rs src/invites.rs src/run.rs src/stats.rs
```

Expected: `bot.rs` around 1600, down from 2740.

- [ ] **Step 3: Full verification**

```bash
cargo test
cargo clippy --all-targets
```

Expected: 436 passed, 3 ignored; clippy silent.

- [ ] **Step 4: Deploy**

```bash
docker compose exec -T scout sh -c 'cp -a /data/scout.duckdb /data/scout.duckdb.pre-2b1'
CARGO_BUILD_JOBS=4 docker compose up -d --build
docker compose logs --tail 20 scout | grep -E 'who may talk|scout is up|ERROR|panicked'
docker inspect scout-scout-1 --format 'restarts={{.RestartCount}}'
```

Expected: `schema=5`, `restarts=0`, no errors. This phase adds no migration
steps, so any `applied migration step` line means something was added that
should not have been.

- [ ] **Step 5: Exercise every moved path in a real chat**

Each of these took a different route through `bot.rs` before this plan and a
different one after, and no test covers the seam between parsing and acting:

1. an ordinary question that uses a tool
2. `/stat`
3. `/invite status`
4. a photo
5. `/reset`, then a follow-up, to confirm the conversation actually restarted

- [ ] **Step 6: Commit the measurement**

```bash
git commit --allow-empty -m "chore: the adapter asks core for five things, down from 28"
```

---

## Rollback

No schema change, so rollback is code only:

```bash
git revert <merge commit>
CARGO_BUILD_JOBS=4 docker compose up -d --build
```

## What this plan deliberately leaves out

**The cargo workspace.** The option chosen was "three crates *and* the
handlers move into core", and this plan does only the second half.

The reason is that the order matters and the sizes are lopsided. Moving the
logic is the part that needs judgement — deciding what is transport and what
is Scout — and it is verifiable at every step by 430 existing tests. Drawing
the crate boundary afterwards is a file move plus a `Cargo.toml`, because by
then each module already sits on the correct side. Doing both at once would
roughly double this plan and produce a diff in which a genuine mistake and a
thousand lines of relocation look identical.

So the boundary is enforced by measurement here (Task 10) and by the
compiler in 2b-2, where the crates have to exist anyway because core becomes
its own binary. If that trade is not wanted, the crate split is a separate
short plan that can run immediately after this one.

## What 2b-2 inherits

- The surviving five calls are the API. `2b-2` turns each into an endpoint
  rather than inventing a surface from scratch.
- `Core` is what the HTTP service will own; `App` is what the adapter keeps.
  The split is already drawn, so 2b-2 moves files across a crate boundary
  rather than deciding where things go.
- `plan_announcement` / `record_announcement` and `advert_targets` are
  already two-phase, which is the shape a network forces. Doing it now meant
  doing it without a network to debug at the same time.
