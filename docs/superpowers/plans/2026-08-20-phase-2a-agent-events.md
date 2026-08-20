# Phase 2a — The Agent Stops Owning the Renderer: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `run_agent` emits events describing what it is doing instead of
driving a Telegram message editor, and the adapter renders them — still one
binary, still one process, no change a user can see.

**Architecture:** A small `AgentEvent` enum travels over an in-process
unbounded channel. `run_agent` takes the sending half by value and drops it
on return, which closes the channel; a renderer loop consumes the receiving
half and drives `Live`. The two run under `tokio::join!` rather than
`tokio::spawn`, so no `Send` bound is needed on the renderer's futures. In
phase 2b the channel becomes an SSE socket and nothing else about this
changes.

**Tech Stack:** Rust 1.96 / edition 2021, tokio mpsc, async-fn-in-trait
(stable since 1.75, used generically so dyn-safety never arises).

---

## Verified before writing this plan

| Question | Answer | How |
|---|---|---|
| `Live`'s whole public surface | `new`, `message_id`, `shown`, `show`, `show_thinking` | `src/progress.rs` |
| Renderer calls **inside** `run_agent` | exactly 5 | `src/bot.rs:1791, 1800, 1810, 1823, 1860` |
| Renderer calls outside it | `Live::new` ×2, error frame ×2, and `deliver` — all adapter-side already | `src/bot.rs:1483, 1491, 1699, 1704, 1994-2011` |
| async fn in trait, used generically | compiles on 1.96 | probe |
| `tokio::join!(producer(tx), render(r, rx))` terminates | yes — `tx` moves into the producer and drops on return, closing the channel | probe |
| Ordering preserved through the channel | yes — probe recorded `[tool, thinking, answer]` in order | probe |
| Test suite before starting | 425 passing, 3 ignored | `cargo test` |

`tokio::spawn` was rejected deliberately. Spawning requires the renderer's
futures to be `Send`, which async-fn-in-trait cannot express without
`async_trait` or hand-written `impl Future + Send` returns. `join!` runs both
futures in the caller's task and needs neither.

## The five events, and where each comes from

| `src/bot.rs` today | Becomes |
|---|---|
| `1791` `live.show(describe(tool, args), false)` | `AgentEvent::Tool(String)` |
| `1800` `live.show(&answer, false)` | `AgentEvent::Answer(String)` |
| `1810` `live.show_thinking(&thinking)` | `AgentEvent::Thinking(String)` |
| `1823` `live.show_thinking(&thinking)` | `AgentEvent::Thinking(String)` |
| `1860` `live.show("✍️ wrapping up …", false)` | `AgentEvent::Notice(String)` |

`Tool` and `Notice` both render identically today. They stay separate
because they mean different things and phase 2b's wire format names `tool`
as its own event; collapsing them would have to be undone.

Every variant carries the **whole** text, not a delta, because that is what
`Live::show` already receives and `Live` does its own diffing against
`shown()`. Deltas would suit a socket better and are a 2b question — doing
it here would change behaviour while this plan claims not to.

## File structure

| File | Responsibility | Change |
|---|---|---|
| `src/events.rs` | the event vocabulary and the sink type | **create** (~40 lines) |
| `src/progress.rs` | rendering: the `Renderer` trait, `Live`'s impl, the event loop | modify (+~70) |
| `src/bot.rs` | `run_agent` emits; the two call sites join producer and renderer | modify (~30 lines changed) |
| `src/main.rs` | `mod events;` | modify (1 line) |

`events.rs` is deliberately its own file rather than a corner of `bot.rs`:
in 2b it is the crate both binaries depend on, and starting it as a separate
module means that move is a file rename rather than an extraction.

---

## Task 1: The event vocabulary

**Files:**
- Create: `src/events.rs`
- Modify: `src/main.rs:1-12` (module list)
- Test: `src/events.rs`

- [ ] **Step 1: Write the failing test**

Create `src/events.rs` containing only the tests for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emitting_into_a_closed_channel_is_not_an_error() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        drop(rx);
        // Nobody is listening. A run that is still doing useful work must
        // not be brought down because the chat went away.
        emit(&tx, AgentEvent::Answer("still working".to_string()));
    }

    #[test]
    fn events_arrive_in_the_order_they_were_sent() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        emit(&tx, AgentEvent::Tool("searching Kagi".to_string()));
        emit(&tx, AgentEvent::Thinking("comparing".to_string()));
        emit(&tx, AgentEvent::Answer("The cheapest".to_string()));
        drop(tx);

        let mut got = Vec::new();
        while let Ok(e) = rx.try_recv() {
            got.push(e);
        }
        assert_eq!(
            got,
            vec![
                AgentEvent::Tool("searching Kagi".to_string()),
                AgentEvent::Thinking("comparing".to_string()),
                AgentEvent::Answer("The cheapest".to_string()),
            ]
        );
    }
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test emitting_into_a_closed_channel`
Expected: FAIL — `cannot find type 'AgentEvent' in this scope` (the module
is not declared yet either, so the compiler may not reach the test at all;
either way it must not pass).

- [ ] **Step 3: Write the implementation**

Put this **above** the test module in `src/events.rs`:

```rust
/// What the agent has to say while it works, independent of who is
/// listening.
///
/// Every variant carries the whole text rather than a delta, because that is
/// what `Live::show` already takes and `Live` diffs it against what is on
/// screen. A socket would rather have deltas; that is a phase-2b question,
/// and changing it here would alter behaviour while claiming not to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    /// A tool started, already rendered as a human sentence.
    Tool(String),
    /// The answer so far, with reasoning stripped out.
    Answer(String),
    /// Reasoning so far. Shown only while the answer is still empty.
    Thinking(String),
    /// A line from the run itself rather than from the model — today only
    /// the wrap-up notice when a run is salvaged.
    Notice(String),
}

/// The sending half. `run_agent` takes one by value so that returning drops
/// it, which closes the channel and ends the renderer.
pub type EventSink = tokio::sync::mpsc::UnboundedSender<AgentEvent>;

/// Hands an event to whoever is listening, if anyone is.
///
/// A send fails only when the receiver has gone, which means nobody is
/// watching this run any more. That is not a reason to abandon work the
/// user may still be charged for, so the error is dropped on purpose.
pub fn emit(sink: &EventSink, event: AgentEvent) {
    let _ = sink.send(event);
}
```

Add the module to `src/main.rs`, in alphabetical position between `draft`
and `links`:

```rust
mod events;
```

- [ ] **Step 4: Run the tests**

Run: `cargo test events::tests`
Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
git add src/events.rs src/main.rs
git commit -m "feat: a vocabulary for what the agent is doing"
```

---

## Task 2: A renderer that is not necessarily Telegram

**Files:**
- Modify: `src/progress.rs` (append trait, impl, loop and tests)
- Test: `src/progress.rs`

- [ ] **Step 1: Write the failing test**

Append to `src/progress.rs`:

```rust
#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::events::AgentEvent;

    /// A renderer that writes to a list instead of to Telegram. This is the
    /// thing phase two was for: progress rendering can now be tested with no
    /// bot token, no network and no rate limiter.
    #[derive(Default)]
    struct Recorder {
        frames: Vec<(String, bool)>,
    }

    impl Renderer for Recorder {
        async fn render(&mut self, text: &str, _force: bool) -> bool {
            self.frames.push((text.to_string(), false));
            true
        }
        async fn render_thinking(&mut self, text: &str) -> bool {
            self.frames.push((text.to_string(), true));
            true
        }
    }

    #[tokio::test]
    async fn every_event_reaches_the_renderer_in_order_and_in_the_right_mode() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        crate::events::emit(&tx, AgentEvent::Tool("searching Kagi".into()));
        crate::events::emit(&tx, AgentEvent::Thinking("comparing fares".into()));
        crate::events::emit(&tx, AgentEvent::Answer("The cheapest".into()));
        crate::events::emit(&tx, AgentEvent::Notice("wrapping up".into()));
        drop(tx);

        let rec = render_events(Recorder::default(), rx).await;
        assert_eq!(
            rec.frames,
            vec![
                ("searching Kagi".to_string(), false),
                ("comparing fares".to_string(), true),
                ("The cheapest".to_string(), false),
                ("wrapping up".to_string(), false),
            ],
            "thinking renders in its own mode; everything else is ordinary text"
        );
    }

    #[tokio::test]
    async fn the_renderer_is_handed_back_when_the_run_ends() {
        // The caller needs it afterwards to send the final answer, so the
        // loop must return it rather than consume it.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        crate::events::emit(&tx, AgentEvent::Answer("done".into()));
        drop(tx);
        let rec = render_events(Recorder::default(), rx).await;
        assert_eq!(rec.frames.len(), 1);
    }

    #[tokio::test]
    async fn a_run_that_says_nothing_still_returns() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        drop(tx);
        let rec = render_events(Recorder::default(), rx).await;
        assert!(rec.frames.is_empty());
    }
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test every_event_reaches_the_renderer`
Expected: FAIL — `cannot find trait 'Renderer' in this scope`

- [ ] **Step 3: Write the implementation**

Append to `src/progress.rs`, above the test module:

```rust
/// Somewhere progress can be shown.
///
/// `Live` is the real one. A test can supply a different one, which is what
/// makes the pacing and flood-control path reachable without a bot token.
///
/// Async fn in a trait, used only through generics — this is never a
/// `dyn Renderer`, so its futures needing no `Send` bound costs nothing.
/// Named `render*` rather than `show*` on purpose. `Live` already has
/// inherent `show`/`show_thinking`, and a trait method of the same name
/// would make `self.show(..)` inside the impl depend on inherent methods
/// winning name resolution. If that ever resolved the other way it would be
/// unbounded recursion — a stack overflow at runtime, not a compile error.
/// Different names cannot go wrong.
pub trait Renderer {
    /// Returns whether anything was actually shown.
    async fn render(&mut self, text: &str, force: bool) -> bool;
    async fn render_thinking(&mut self, text: &str) -> bool;
}

impl Renderer for Live {
    async fn render(&mut self, text: &str, force: bool) -> bool {
        self.show(text, force).await
    }
    async fn render_thinking(&mut self, text: &str) -> bool {
        self.show_thinking(text).await
    }
}

/// Draws every event the run produces, then hands the renderer back.
///
/// Ends when the channel closes, which happens when `run_agent` returns and
/// drops its sink — so the loop needs no shutdown signal of its own.
pub async fn render_events<R: Renderer>(
    mut renderer: R,
    mut events: tokio::sync::mpsc::UnboundedReceiver<crate::events::AgentEvent>,
) -> R {
    use crate::events::AgentEvent;
    while let Some(event) = events.recv().await {
        match event {
            AgentEvent::Tool(text) | AgentEvent::Answer(text) | AgentEvent::Notice(text) => {
                renderer.render(&text, false).await;
            }
            AgentEvent::Thinking(text) => {
                renderer.render_thinking(&text).await;
            }
        }
    }
    renderer
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test render_tests`
Expected: 3 passed.

- [ ] **Step 5: Prove the guard by breaking it**

Change the `AgentEvent::Thinking` arm to call `renderer.render(&text, false)`
instead. Run `cargo test every_event_reaches_the_renderer` and confirm it
fails on the `("comparing fares", true)` entry. Restore it.

- [ ] **Step 6: Commit**

```bash
git add src/progress.rs
git commit -m "feat: progress can be drawn somewhere that is not telegram"
```

---

## Task 3: The agent emits instead of drawing

**Files:**
- Modify: `src/bot.rs:1733-1746` (signature), `1791`, `1800`, `1810`, `1823`, `1860`

- [ ] **Step 1: Change the signature**

Replace the doc comment and signature of `run_agent`:

```rust
/// Runs the agent, reporting progress as events rather than drawing them.
///
/// `events` is taken by value: returning drops it, which closes the channel
/// and ends whoever is rendering. That is the only shutdown signal the
/// renderer gets, so it must not be held anywhere else.
async fn run_agent(
    app: &App,
    events: crate::events::EventSink,
    user_id: i64,
    chat_id: i64,
    conversation_id: i64,
    prompt: &str,
) -> anyhow::Result<String> {
```

- [ ] **Step 2: Replace the five call sites**

At `src/bot.rs:1791`, the tool line:

```rust
                MultiTurnStreamItem::ToolExecutionStart { tool_call, .. } => {
                    let args = &tool_call.function.arguments;
                    crate::events::emit(
                        &events,
                        crate::events::AgentEvent::Tool(crate::progress::describe(
                            &tool_call.function.name,
                            args,
                        )),
                    );
                }
```

At `1800`, the answer:

```rust
                    let answer = strip_thinking(&streamed);
                    if !answer.is_empty() {
                        crate::events::emit(&events, crate::events::AgentEvent::Answer(answer));
                    }
```

At `1810` and `1823`, both reasoning arms — the same two lines in each:

```rust
                    if strip_thinking(&streamed).is_empty() {
                        crate::events::emit(
                            &events,
                            crate::events::AgentEvent::Thinking(thinking.clone()),
                        );
                    }
```

At `1860`, the wrap-up notice:

```rust
        crate::events::emit(
            &events,
            crate::events::AgentEvent::Notice("✍️ wrapping up with what I found so far".to_string()),
        );
```

Note that all five were `.await`ed before and none are now — emitting is a
synchronous push onto a queue. The agent no longer waits for Telegram, which
is the point.

- [ ] **Step 3: Build and read what the compiler objects to**

Run: `cargo build 2>&1 | head -30`
Expected: two errors, both at the call sites in `handle_text` and
`handle_reaction` — `expected EventSink, found &mut Live`. Task 4 fixes
them. Any error inside `run_agent` itself means a call site was missed.

- [ ] **Step 4: Do not commit; continue to Task 4**

The crate does not build between these two tasks — `run_agent` now wants a
sink and its callers still pass a `Live`. Task 3 and Task 4 land as one
commit at the end of Task 4.

---

## Task 4: Producer and renderer, side by side

**Files:**
- Modify: `src/bot.rs:1483-1493` (`handle_text`), `src/bot.rs:1699-1706` (`handle_reaction`)

- [ ] **Step 1: Rewrite the `handle_text` call site**

```rust
    let (events, incoming) = tokio::sync::mpsc::unbounded_channel();
    let live = Live::new(bot.clone(), chat_id, app.streams.clone());
    // join! rather than spawn: both futures run in this task, so the
    // renderer's futures need no Send bound. `events` moves into the run and
    // drops when it returns, which is what ends the renderer.
    let (result, mut live) = tokio::join!(
        run_agent(&app, events, user_id, chat_id.0, conversation_id, &prompt),
        crate::progress::render_events(live, incoming),
    );
    match result {
        Ok(reply) => deliver(&bot, &app, &mut live, chat_id, &reply).await?,
        Err(e) => {
            tracing::error!(error = %e, chat_id = chat_id.0, "agent request failed");
            // Replace the progress message rather than sending a second one:
            // otherwise the user is left with a half-written thought frozen
            // above the apology.
            live.show(agent_error_message(&e), true).await;
        }
    }
    Ok(())
}
```

- [ ] **Step 2: Rewrite the `handle_reaction` call site**

The same shape, with that function's own error message:

```rust
    let (events, incoming) = tokio::sync::mpsc::unbounded_channel();
    let live = Live::new(bot.clone(), chat_id, app.streams.clone());
    let (result, mut live) = tokio::join!(
        run_agent(&app, events, user_id, chat_id.0, conversation_id, &prompt),
        crate::progress::render_events(live, incoming),
    );
    match result {
        Ok(reply) => deliver(&bot, &app, &mut live, chat_id, &reply).await?,
        Err(e) => {
            tracing::error!(error = %e, chat_id = chat_id.0, "reaction follow-up failed");
            live.show(agent_error_message(&e), true).await;
        }
    }
```

- [ ] **Step 3: Build**

Run: `cargo build 2>&1 | head -20`
Expected: clean, no warnings. If `live` is reported as moved, the `join!`
tuple is being destructured wrongly — `render_events` returns the renderer
and that binding is the one to use afterwards.

- [ ] **Step 4: Run the whole suite**

Run: `cargo test`
Expected: 425 existing tests plus 5 new ones = **430 passed**, 3 ignored.
Nothing should have changed for any existing test: this task moves where a
string is drawn, not what it says.

- [ ] **Step 5: Commit**

```bash
git add src/bot.rs
git commit -m "feat: the agent reports what it is doing instead of drawing it"
```

---

## Task 5: Confirm nothing changed where it counts

The whole claim of 2a is "no behaviour change". Tests cannot show that on
their own, because no test drives a real Telegram message.

**Files:** none — this is verification.

- [ ] **Step 1: Check for a stuck run**

Run: `cargo test`
Expected: 430 passed. A hang here rather than a failure means the channel is
not closing — `events` is being cloned or held past `run_agent`'s return.

- [ ] **Step 2: Back up the live database from inside the container**

Not because this task migrates anything — it does not — but because the
deploy recreates the container and the backup is cheap.

```bash
docker compose exec -T scout sh -c 'cp -a /data/scout.duckdb /data/scout.duckdb.pre-2a'
docker compose exec -T scout sh -c 'ls -la /data/'
```

Never open that file from the host: the host's DuckDB is 1.4.5 and the
container's is v1.5.1.

- [ ] **Step 3: Deploy**

```bash
CARGO_BUILD_JOBS=4 docker compose up -d --build
```

- [ ] **Step 4: Confirm a clean start**

```bash
docker compose logs --tail 20 scout | grep -E 'who may talk|scout is up|ERROR|panicked'
docker inspect scout-scout-1 --format 'restarts={{.RestartCount}}'
```

Expected: `who may talk to this bot founders=8 members=1 daily_cap=20
schema=5`, then `scout is up`, and `restarts=0`. No migration lines — this
phase adds no steps, so `schema` must still read 5.

- [ ] **Step 5: Watch one real run**

Ask the bot something that uses a tool, e.g. *"cheapest USB-C hub"*, and
watch the message in Telegram. Three things must still be true:

1. The progress message names the tool that is running.
2. Reasoning appears in italics before the answer, and is replaced by it.
3. The answer streams into the same message rather than arriving as a new one.

If progress now lags noticeably behind the answer, the renderer is falling
behind the queue — report it rather than tuning blind; `Live` already skips
frames it decides are too frequent, so a visible backlog would mean
something else is wrong.

---

## Rollback

Nothing about the database changes, so rollback is only code:

```bash
git revert <merge commit>
CARGO_BUILD_JOBS=4 docker compose up -d --build
```

The `scout.duckdb.pre-2a` copy exists as ordinary caution, not because this
phase can corrupt anything.

## What 2b inherits from this

- `src/events.rs` becomes the shared `scout-api` crate — a file move.
- `render_events` keeps its shape; only its input changes, from an mpsc
  receiver to a stream of SSE frames.
- `AgentEvent` gains a `seq` for reconnection, and `Answer` may become a
  delta, both of which are wire concerns that do not exist yet.
- `run_agent` needs no further change at all: it already reports rather than
  draws, which was the point of doing this first.
