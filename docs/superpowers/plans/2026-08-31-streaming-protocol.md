# A Streaming Protocol That Can Retract — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `AgentEvent` carry text *updates* rather than whole text, so the same stream can serve a browser without putting megabytes on the wire — without ever leaving a client showing text that core has retracted.

**Architecture:** `TextUpdate::{Append, Replace}` in `scout-api`, produced by a `Shown` accumulator and consumed by an `apply` method. `run_agent` emits updates; Telegram's `render_events` accumulates them and renders whole text into the unchanged `Live`. Separately, a `DashSet` of in-flight conversations stops two runs on one thread erasing each other's history.

**Tech Stack:** Rust workspace (`scout-api`, `scout-core`, `scout-telegram`, `scout-web`), `dashmap`, `rig` for the agent, `teloxide` for Telegram.

---

## Read this first

**`Replace` is a security requirement, not an optimisation.** `strip_thinking`
is not monotonic. Measured, feeding it growing prefixes of one stream:

```
leak 21 -> "secret reasoning here"      emitted as answer
leak 29 -> ""                           retracted once </think> completes
leak 39 -> "The answer"
```

A closer with no opener means the text began inside a thinking block, so
everything before it is reasoning and is discarded. An append-only client
would print `"secret reasoning here"` and never take it back — the leak
`strip_thinking` exists to prevent, reintroduced one layer up. Telegram is
safe today only because `Live::show` takes whole text and replaces.

If at any point you find yourself removing `Replace`, or reinstating an
`if !answer.is_empty()` guard, stop: that guard suppresses exactly the
retraction event.

**Same acceptance rule as the account-keying refactor.** Changing
`AgentEvent`'s shape forces mechanical edits to tests that construct it. So:

> **No test's assertions or expected values change.** Only identifiers and
> constructor shapes change, and only where a type changed.

Known forced edits, listed so an unexpected one is visible:

| file | what |
|---|---|
| `crates/scout-telegram/src/progress.rs` | exactly 3 constructions in `mod tests` wrap their argument in `TextUpdate::Append(..)`: `Thinking("comparing fares")` and `Answer("The cheapest")` in `every_event_reaches_the_renderer_in_order_and_in_the_right_mode`, and `Answer("done")` in `the_renderer_is_handed_back_when_the_run_ends`. `Tool` and `Notice` are untouched. |

**`every_event_reaches_the_renderer_in_order_and_in_the_right_mode` is the
behaviour-preservation proof.** It asserts the exact sequence of frames the
renderer produced. Its expected vector must stay byte-identical through this
whole plan — only the three event *constructions* change. If that expectation
needs editing, Telegram's output changed and the refactor failed.

Anything beyond that is a finding — report it, do not adjust it.

## Verification commands

The toolchain is not pinned and `cargo` / `cargo-clippy` have resolved to
mismatched versions before, producing `E0514` on unrelated crates. Check:

```bash
cargo-clippy --version   # must match:
rustc --version
```

If they disagree:

```bash
PATH="/run/current-system/sw/bin:$PATH" cargo clippy --workspace --all-targets
```

Baseline: **561 passing, 3 ignored** across the workspace.

**Never run `cargo fmt`.** This repository is deliberately not
rustfmt-formatted.

## File structure

| file | change |
|---|---|
| `crates/scout-api/src/lib.rs` | **add** `TextUpdate`, its `apply`, and `Shown`; change two `AgentEvent` variants |
| `crates/scout-core/src/run.rs` | emit updates; add the in-flight guard; return `RunOutcome` |
| `crates/scout-core/src/agent.rs` | `AgentDeps` gains the in-flight set |
| `crates/scout-core/src/core.rs` | construct the in-flight set in `Core::start` |
| `crates/scout-telegram/src/progress.rs` | accumulate updates before rendering |
| `crates/scout-telegram/src/bot.rs` | handle `RunOutcome::Busy` at two call sites |

---

### Task 1: `TextUpdate`, `apply`, and `Shown`

The whole protocol, with no caller yet. Producer and consumer land together
so they can be tested as a round trip.

**Files:**
- Modify: `crates/scout-api/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

Add inside the existing `#[cfg(test)] mod tests` in
`crates/scout-api/src/lib.rs` (there is one already; do not create a second):

```rust
    #[test]
    fn growing_text_produces_appends_and_shrinking_text_produces_a_replace() {
        let mut shown = Shown::default();
        assert_eq!(shown.update("Here"), Some(TextUpdate::Append("Here".into())));
        assert_eq!(shown.update("Here are"), Some(TextUpdate::Append(" are".into())));
        // Not an extension, so the client has to be told to start over.
        assert_eq!(shown.update("Hello"), Some(TextUpdate::Replace("Hello".into())));
    }

    #[test]
    fn text_that_did_not_change_produces_no_event_at_all() {
        // Otherwise every streamed token inside a <think> block would send
        // an empty Append.
        let mut shown = Shown::default();
        shown.update("same");
        assert_eq!(shown.update("same"), None);
    }

    #[test]
    fn becoming_empty_is_a_replace_and_not_silence() {
        // The retraction. `strip_thinking` discards everything before a
        // stray closer, so the answer can go from text to nothing, and a
        // client that is not told will keep showing reasoning.
        let mut shown = Shown::default();
        shown.update("secret reasoning here");
        assert_eq!(shown.update(""), Some(TextUpdate::Replace(String::new())));
    }

    #[test]
    fn applying_updates_in_order_reproduces_the_text_that_produced_them() {
        let mut shown = Shown::default();
        let mut client = String::new();
        for step in ["a", "ab", "abc", "xyz", "", "done"] {
            if let Some(update) = shown.update(step) {
                update.apply(&mut client);
            }
            assert_eq!(client, step, "client drifted from the source text");
        }
    }
```

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p scout-api
```

Expected: compile errors — `cannot find type TextUpdate`, `cannot find type Shown`.

- [ ] **Step 3: Add the types**

Add to `crates/scout-api/src/lib.rs`, before `AgentEvent`:

```rust
/// How a piece of streamed text changed.
///
/// `Replace` is not an optimisation escape hatch — it is required. The
/// answer can *shrink*: `strip_thinking` discards everything before a
/// closing tag that has no opener, because such a closer means the text
/// began inside a thinking block. A client that only ever appends would go
/// on showing reasoning the run has already retracted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TextUpdate {
    Append(String),
    Replace(String),
}

impl TextUpdate {
    /// Moves accumulated text forward by this update.
    ///
    /// The counterpart of `Shown::update`. Every client does exactly this,
    /// which is why it lives here rather than being written twice.
    pub fn apply(&self, into: &mut String) {
        match self {
            TextUpdate::Append(delta) => into.push_str(delta),
            TextUpdate::Replace(text) => {
                into.clear();
                into.push_str(text);
            }
        }
    }
}

/// What a client has been shown so far, and what to send it next.
///
/// Producers hold one of these per stream of text and feed it the whole
/// text each time; it works out the smallest honest update. `None` means
/// nothing changed and no event is worth sending.
#[derive(Debug, Default, Clone)]
pub struct Shown(String);

impl Shown {
    pub fn update(&mut self, next: &str) -> Option<TextUpdate> {
        if next == self.0 {
            return None;
        }
        let update = match next.strip_prefix(self.0.as_str()) {
            Some(rest) => TextUpdate::Append(rest.to_string()),
            None => TextUpdate::Replace(next.to_string()),
        };
        self.0 = next.to_string();
        Some(update)
    }
}
```

- [ ] **Step 4: Run them and watch them pass**

```bash
cargo test -p scout-api
```

Expected: 8 passing (4 existing + 4 new).

- [ ] **Step 5: Commit**

```bash
git add crates/scout-api
git commit -m "feat: streamed text travels as an update that can retract"
```

Append these trailers to this and every commit in this plan:

```
Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01BSFg94PYLWoB4pp2Bd6QzF
```

---

### Task 2: Prove it against the real `strip_thinking`

The property the whole design rests on, tested before anything depends on
it. This lands with no wiring changed, so if it fails the design is wrong
rather than the integration.

**Files:**
- Modify: `crates/scout-core/src/text.rs`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `crates/scout-core/src/text.rs`:

```rust
    #[test]
    fn a_client_fed_updates_sees_exactly_what_strip_thinking_says_at_every_step() {
        // The load-bearing property. A stream arrives one character at a
        // time; at every prefix, a client that applied the updates must be
        // showing precisely what `strip_thinking` would have shown. The
        // second case is the one that matters: a closer with no opener
        // means everything before it was reasoning, so the client has to be
        // told to throw away what it already displayed.
        for source in [
            "Here are three bikes<think>actually let me reconsider",
            "secret reasoning here</think>The answer",
        ] {
            let mut shown = scout_api::Shown::default();
            let mut client = String::new();
            for (i, _) in source.char_indices().chain(std::iter::once((source.len(), ' '))) {
                let answer = strip_thinking(&source[..i]);
                if let Some(update) = shown.update(&answer) {
                    update.apply(&mut client);
                }
                assert_eq!(
                    client, answer,
                    "client drifted at prefix {i:?} of {source:?}"
                );
            }
        }
    }

    #[test]
    fn a_stray_closer_retracts_what_was_already_shown() {
        // Named separately because this is the security property, not a
        // formatting one: without the Replace the client keeps the
        // reasoning on screen.
        let source = "secret reasoning here</think>The answer";
        let mut shown = scout_api::Shown::default();
        // Everything up to the closer is shown as answer text.
        assert!(matches!(
            shown.update(&strip_thinking(&source[..21])),
            Some(scout_api::TextUpdate::Append(ref t)) if t == "secret reasoning here"
        ));
        // The completed closer retracts all of it.
        assert_eq!(
            shown.update(&strip_thinking(&source[..29])),
            Some(scout_api::TextUpdate::Replace(String::new())),
            "the retraction was not sent, so a client would still be showing reasoning"
        );
    }
```

- [ ] **Step 2: Run and watch them pass**

```bash
cargo test -p scout-core strip_thinking
cargo test -p scout-core a_client_fed_updates a_stray_closer
```

These should pass immediately — Task 1 built the machinery. If either
fails, the design is wrong and you should stop and report it rather than
adjusting the test.

- [ ] **Step 3: Commit**

```bash
git add crates/scout-core/src/text.rs
git commit -m "test: a client fed updates never shows what strip_thinking retracted"
```

---

### Task 3: `AgentEvent` carries updates, and `run_agent` produces them

This task deliberately leaves `scout-telegram` not compiling. Task 4 fixes
it. Verify with `-p scout-core` only.

**Files:**
- Modify: `crates/scout-api/src/lib.rs`
- Modify: `crates/scout-core/src/run.rs`

- [ ] **Step 1: Change the two variants**

In `crates/scout-api/src/lib.rs`:

```rust
pub enum AgentEvent {
    /// A tool started, already rendered as a human sentence. Whole text:
    /// one discrete sentence, not a growing one.
    Tool(String),
    /// The answer, as it changes.
    Answer(TextUpdate),
    /// Reasoning, as it changes. Shown only while the answer is empty.
    Thinking(TextUpdate),
    /// A line from the run itself rather than from the model — today only
    /// the wrap-up notice when a run is salvaged.
    Notice(String),
}
```

Remove the paragraph in the type's doc comment that says every variant
carries whole text rather than a delta, and that a socket would rather have
deltas — that is what this change settles, so leaving it would be a comment
that contradicts its own type. Replace it with a sentence pointing at
`TextUpdate` for why an update can be a `Replace`.

- [ ] **Step 2: Produce updates in `run_agent`**

In `crates/scout-core/src/run.rs`, next to the existing `streamed` and
`thinking` accumulators near the top of the run, add two producers:

```rust
    // What each client has been shown, so the run can send the smallest
    // honest update rather than the whole text every token.
    let mut answer_shown = scout_api::Shown::default();
    let mut thinking_shown = scout_api::Shown::default();
```

Replace the `Text` arm's body:

```rust
                MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(t)) => {
                    streamed.push_str(&t.text);
                    // Unclosed <think> blocks render as nothing, so inline
                    // reasoning never reaches the chat as answer text — and
                    // when a stray closer proves the text so far *was*
                    // reasoning, the update is a Replace that takes it back.
                    // The old `if !answer.is_empty()` guard suppressed
                    // exactly that event.
                    if let Some(update) = answer_shown.update(&strip_thinking(&streamed)) {
                        scout_api::emit(&events, scout_api::AgentEvent::Answer(update));
                    }
                }
```

Replace both `Thinking` emissions — the `ReasoningDelta` arm and the
`Reasoning` arm — with the same shape, keeping each arm's existing
accumulation of `thinking` and its existing "only while the answer is empty"
guard:

```rust
                    if strip_thinking(&streamed).is_empty() {
                        if let Some(update) = thinking_shown.update(&thinking) {
                            scout_api::emit(&events, scout_api::AgentEvent::Thinking(update));
                        }
                    }
```

- [ ] **Step 3: Verify core**

```bash
cargo check -p scout-core
cargo test -p scout-core
```

Expected: clean, and **447 passing, 3 ignored** (445 before, plus Task 2's
two tests).

```bash
cargo check -p scout-telegram 2>&1 | grep -c "^error"
```

Expected: non-zero, all about `AgentEvent` patterns in `progress.rs`. Do not
fix them here. Report the count.

- [ ] **Step 4: Commit**

```bash
git add crates/scout-api/src/lib.rs crates/scout-core/src/run.rs
git commit -m "refactor: a run reports how its text changed, not all of it"
```

Say in the body that `scout-telegram` does not compile until the next
commit, so a reader bisecting knows it is deliberate.

---

### Task 4: Telegram accumulates

**Files:**
- Modify: `crates/scout-telegram/src/progress.rs`

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/scout-telegram/src/progress.rs`:

The module already has a test renderer, `Recorder`, which records every
frame as `frames: Vec<(String, bool)>` where the bool marks thinking mode.
Use it:

```rust
    #[tokio::test]
    async fn a_retracted_answer_is_not_left_on_screen() {
        // The reason the protocol has a Replace at all. If the renderer only
        // appended, reasoning would stay on screen after the run decided it
        // was reasoning — the leak `strip_thinking` exists to prevent.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        scout_api::emit(&tx, AgentEvent::Answer(TextUpdate::Append("secret reasoning".into())));
        scout_api::emit(&tx, AgentEvent::Answer(TextUpdate::Replace(String::new())));
        scout_api::emit(&tx, AgentEvent::Answer(TextUpdate::Append("The answer".into())));
        drop(tx);

        let rec = render_events(Recorder::default(), rx).await;
        assert_eq!(
            rec.frames,
            vec![
                ("secret reasoning".to_string(), false),
                (String::new(), false),
                ("The answer".to_string(), false),
            ],
            "a Replace must clear what was shown, not extend it"
        );
    }
```

Add `TextUpdate` to the module's `scout_api` import if it is not already in
scope — the tests use `AgentEvent` unqualified, so follow whatever form that
import already takes.

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p scout-telegram
```

Expected: compile errors first (the match arms are still whole-text), which
is the failure that matters here.

- [ ] **Step 3: Accumulate**

Replace the match in `render_events`:

```rust
    use scout_api::AgentEvent;
    // What the answer has grown to. `Tool` and `Notice` deliberately do not
    // touch it: each is a one-off sentence that momentarily replaces the
    // display, and the next answer update re-renders the whole answer over
    // it — which is exactly what the whole-text protocol used to do.
    let mut answer = String::new();
    let mut thinking = String::new();
    while let Some(event) = events.recv().await {
        match event {
            AgentEvent::Tool(text) | AgentEvent::Notice(text) => {
                renderer.render(&text, false).await;
            }
            AgentEvent::Answer(update) => {
                update.apply(&mut answer);
                renderer.render(&answer, false).await;
            }
            AgentEvent::Thinking(update) => {
                update.apply(&mut thinking);
                renderer.render_thinking(&thinking).await;
            }
        }
    }
```

- [ ] **Step 4: Fix the forced test constructions**

The existing tests in that module construct `AgentEvent::Answer("done".into())`
and similar. Wrap each argument: `AgentEvent::Answer(TextUpdate::Append("done".into()))`.
**Change no assertion and no expected string.**

- [ ] **Step 5: Verify the workspace**

```bash
cargo test --workspace
```

Expected: 561 baseline plus Task 1's four, Task 2's two and this task's one.
**Verify the real number and report it** rather than trusting that
arithmetic; what matters is that nothing FAILS and nothing disappeared.

- [ ] **Step 6: Commit**

```bash
git add crates/scout-telegram/src/progress.rs
git commit -m "refactor: the renderer accumulates what the run reports"
```

---

### Task 5: One run at a time per conversation

**Files:**
- Modify: `crates/scout-core/src/agent.rs`
- Modify: `crates/scout-core/src/core.rs`
- Modify: `crates/scout-core/src/run.rs`
- Modify: `crates/scout-telegram/src/bot.rs`

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/scout-core/src/run.rs` (create the module if
the file has none):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conversation_admits_one_run_and_frees_itself_when_it_ends() {
        // Two runs on one thread both load the history and both write it
        // back wholesale, so the second erases the first's exchange. A
        // laptop and a phone on the shared `direct` thread make that
        // ordinary rather than rare.
        let running = std::sync::Arc::new(dashmap::DashSet::new());
        let first = begin_run(&running, 7).expect("the first run should start");
        assert!(begin_run(&running, 7).is_none(), "a second run got in");
        // A different thread is unaffected.
        assert!(begin_run(&running, 8).is_some());

        drop(first);
        assert!(
            begin_run(&running, 7).is_some(),
            "the conversation stayed locked after its run ended"
        );
    }
}
```

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p scout-core a_conversation_admits_one_run
```

Expected: `cannot find function begin_run`.

- [ ] **Step 3: Add the guard**

In `crates/scout-core/src/run.rs`:

```rust
/// Held for the length of a run. Dropping it frees the conversation, so a
/// panic, a timeout or a dropped future cannot wedge a thread forever —
/// which an insert/remove pair around the body would.
pub(crate) struct RunGuard {
    running: std::sync::Arc<dashmap::DashSet<i64>>,
    conversation_id: i64,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.running.remove(&self.conversation_id);
    }
}

/// Claims a conversation, or `None` if a run already holds it.
///
/// `DashSet::insert` reports whether the value was new, which makes this an
/// atomic check-and-claim rather than a check followed by a claim.
pub(crate) fn begin_run(
    running: &std::sync::Arc<dashmap::DashSet<i64>>,
    conversation_id: i64,
) -> Option<RunGuard> {
    running
        .insert(conversation_id)
        .then(|| RunGuard { running: running.clone(), conversation_id })
}
```

- [ ] **Step 4: Add the set to `AgentDeps` and construct it**

In `crates/scout-core/src/agent.rs`, add a field to `AgentDeps` beside the
existing `shown`:

```rust
    /// Conversations with a run in flight. See `run::begin_run`.
    pub running: std::sync::Arc<dashmap::DashSet<i64>>,
```

In `crates/scout-core/src/core.rs`, inside `Core::start` where `AgentDeps`
is constructed, add:

```rust
            running: std::sync::Arc::new(dashmap::DashSet::new()),
```

- [ ] **Step 5: Return an outcome from `run_agent`**

In `crates/scout-core/src/run.rs`:

```rust
/// What a run produced.
///
/// `Busy` is not an error: asking two questions at once in one thread is an
/// ordinary thing to do. Each channel words it itself rather than core
/// writing chat copy.
pub enum RunOutcome {
    Answered(String),
    Busy,
}
```

Change the signature to `-> anyhow::Result<RunOutcome>`, claim the
conversation at the top of the body, and return `Answered` at the end:

```rust
    let Some(_guard) = begin_run(&core.deps.running, run.conversation_id) else {
        return Ok(RunOutcome::Busy);
    };
```

Bind it as `_guard` rather than `_`: `let _ = ...` drops it immediately and
the lock would never be held. The final `Ok(reply)` becomes
`Ok(RunOutcome::Answered(reply))`.

- [ ] **Step 6: Handle `Busy` in the adapter**

In `crates/scout-telegram/src/bot.rs`, both `run_agent` call sites match on
the result. Each becomes:

```rust
    match result {
        Ok(scout_core::run::RunOutcome::Answered(reply)) => {
            deliver(&bot, &app, &mut live, chat_id, &reply).await?
        }
        Ok(scout_core::run::RunOutcome::Busy) => {
            live.show("I'm still working on your last message — one moment.", true).await
        }
        Err(e) => { /* unchanged */ }
    }
```

Keep each site's existing `Err` arm exactly as it is; the two differ, and
neither is being changed.

- [ ] **Step 7: Verify**

```bash
cargo test --workspace
PATH="/run/current-system/sw/bin:$PATH" cargo clippy --workspace --all-targets
```

Expected: one more test than after Task 4, and clippy silent.

- [ ] **Step 8: Commit**

```bash
git add crates/scout-core crates/scout-telegram
git commit -m "fix: one run at a time per conversation"
```

---

### Task 6: Verification

- [ ] **Step 1: Full suite and clippy**

```bash
cargo test --workspace
PATH="/run/current-system/sw/bin:$PATH" cargo clippy --workspace --all-targets
```

- [ ] **Step 2: Audit every test-file change**

```bash
git diff main...HEAD -- crates | grep -E "^-.*assert"
```

Every removed assertion must be explainable as a constructor reshape. A
changed expected value is a finding — report it, do not adjust it.

- [ ] **Step 3: Mutation — the retraction**

Reinstate the old guard in `run.rs`'s `Text` arm:

```rust
                    let answer = strip_thinking(&streamed);
                    if !answer.is_empty() {
                        if let Some(update) = answer_shown.update(&answer) {
                            scout_api::emit(&events, scout_api::AgentEvent::Answer(update));
                        }
                    }
```

```bash
cargo test -p scout-core a_stray_closer
```

Expected: **FAIL.** This is the whole point of the design. Revert it.

- [ ] **Step 4: Mutation — the renderer appends across a Replace**

In `progress.rs`, make `Answer` always append:

```rust
            AgentEvent::Answer(update) => {
                if let scout_api::TextUpdate::Append(d) | scout_api::TextUpdate::Replace(d) = &update {
                    answer.push_str(d);
                }
                renderer.render(&answer, false).await;
            }
```

```bash
cargo test -p scout-telegram a_retracted_answer
```

Expected: **FAIL.** Revert it.

- [ ] **Step 5: Mutation — the run guard**

In `run.rs`, bind the guard as `let _ = begin_run(...)` so it drops at once.

```bash
cargo test -p scout-core a_conversation_admits_one_run
```

Expected: **FAIL.** Revert it.

- [ ] **Step 6: Report**

State the final test count, the clippy result, and the outcome of each
mutation. If any mutation *passed*, the corresponding test does not protect
what it claims to and that is a finding.

---

## What this deliberately does not do

- No SSE endpoint, no browser client, no route. That is W3b, which depends
  on this.
- No `seq` numbers and no `Last-Event-ID` replay. History is the source of
  truth once a run ends, so a dropped stream costs the animation rather than
  the answer, and replay would have to reproduce `Replace` exactly or it
  would resurrect retracted reasoning.
- No cancellation endpoint. It belongs with a client that has a stop button.
- `Tool` and `Notice` stay whole text: each is one discrete sentence that
  never grows, so a `TextUpdate` would be ceremony.
