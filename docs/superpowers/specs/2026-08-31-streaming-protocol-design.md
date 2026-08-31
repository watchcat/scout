# A Streaming Protocol That Can Retract — Design

## Purpose

`AgentEvent::Answer` carries the whole answer so far and is re-emitted on
every streamed token. That is right for Telegram, where `Live` diffs it
locally against what is on screen and paces the edits. It is wrong for a
network: a 6 KB answer streamed in 400 chunks puts megabytes on the wire to
deliver 6 KB.

W3's browser client needs the same events over SSE. This changes the protocol
to carry updates rather than whole text, so both channels can share one
stream.

It is a change to shipped code with no new surface. W3b, the web chat, is a
separate spec and depends on this one.

## What this is not

No new endpoint, no page, no route. Nothing a user can see changes: Telegram
renders exactly what it renders today, which is the property the tests are
built around.

## The measurement that shapes everything here

The obvious design — append-only deltas — is unsafe, and not obviously so.
`strip_thinking` is **not monotonic**. Feeding it growing prefixes of one
stream, measured:

```
len  21 -> "Here are three bikes<"      a partial tag is shown
len  40 -> "Here are three bikes"       and then withdrawn

leak 21 -> "secret reasoning here"      emitted as answer
leak 29 -> ""                           retracted once </think> completes
leak 39 -> "The answer"
```

Two separate mechanisms. An unclosed `<think>` opener causes everything after
it to be dropped, so a partially-arrived tag briefly appears and then goes.
And a closer with no opener means the text began *inside* a thinking block,
so everything before it is reasoning and is discarded wholesale —
`strip_thinking`'s own comment records that keeping it once "published a
whole chain of thought into a chat, system prompt and all".

An append-only client shown `leak 21` would print "secret reasoning here" and
have no way to take it back. That is the same leak, reintroduced at the
protocol layer rather than in the parser. **Telegram is safe from it today
only because `Live::show` takes whole text and replaces the message.**

So the protocol must be able to retract. This is a security requirement, not
an efficiency one, and it is why the design below is not simply "deltas".

## The protocol

Both types live in `scout-api`, beside `AgentEvent` itself: they are the
vocabulary two channels share, and W4 puts them on a wire.

```rust
/// How a piece of streamed text changed.
///
/// `Replace` exists because the answer can shrink: see `strip_thinking`.
/// A client that only ever appends would keep text the model has since
/// decided was reasoning.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TextUpdate {
    Append(String),
    Replace(String),
}

pub enum AgentEvent {
    /// A tool started, already rendered as a human sentence. Whole text:
    /// it is one discrete sentence, not a growing one.
    Tool(String),
    Answer(TextUpdate),
    Thinking(TextUpdate),
    /// A line from the run itself rather than the model. Whole text, for
    /// the same reason as `Tool`.
    Notice(String),
}
```

`Replace` fires only at tag boundaries, so a normal answer is one `Append`
per token and the wire cost is linear in the answer, not quadratic.

## Where the update is computed

In `run_agent`, which already holds both the accumulated raw text and the
previous stripped answer. For each streamed chunk:

- compute `next = strip_thinking(&streamed)`
- if `next.starts_with(&previous)` emit `Append(next[previous.len()..])`
- otherwise emit `Replace(next.clone())`
- remember `next` as `previous`

**The current emptiness guard has to go, and this is easy to get wrong.**
Today the code reads `if !answer.is_empty() { emit(...) }`. That guard would
suppress precisely the retraction event, because the retraction *is* the
answer becoming empty. The new rule is to emit whenever `next != previous`,
including when `next` is empty — and a test asserts that the empty case is
emitted rather than skipped.

An `Append("")` is never emitted, because equal strings produce no event at
all.

`Thinking` gets the same treatment against its own accumulator. It is still
only emitted while the answer is empty, which is existing behaviour and is
not being changed here.

## What Telegram does instead

`render_events` currently treats `Tool`, `Answer` and `Notice` alike: each
replaces what is on screen. It gains one accumulator, used by `Answer` only:

- `Answer(Append(d))` — push `d`, render the accumulator
- `Answer(Replace(t))` — assign `t`, render the accumulator
- `Tool(t)` and `Notice(t)` — render `t`, leaving the accumulator alone

That last line preserves today's behaviour exactly: a tool sentence
temporarily replaces the display, and the next answer update re-renders the
whole answer over it. `Live` and its pacing are untouched, because it still
receives whole text.

## One run at a time per conversation

Bundled here rather than in W3b because it is a fix to shipped behaviour, and
because W3b is what makes it urgent.

`run_agent` loads the conversation's history at the start and calls
`replace_messages` at the end, which deletes and rewrites every row. There is
no lock. Two concurrent runs on one conversation therefore both load `H` and
both save `H` plus their own exchange — and the second save erases the
first's. A whole question and answer disappear.

Today that needs two rapid Telegram messages. W3b makes it ordinary: the web
chat shares the `direct` thread, so a phone and a laptop are two clients on
one conversation.

Core keeps a `DashSet<i64>` of conversations with a run in flight, beside the
`ShownFlights` map that already lives in `AgentDeps`. Entry is an RAII guard
that removes the id on drop, so a panicking or cancelled run cannot wedge a
conversation permanently — a plain insert/remove pair would.

`run_agent`'s return type changes from `anyhow::Result<String>` to
`anyhow::Result<RunOutcome>`, so that no caller can forget the case:

```rust
pub enum RunOutcome {
    Answered(String),
    /// Another run holds this conversation.
    Busy,
}
```

`RunOutcome` lives in `scout-core::run` rather than `scout-api`. It is not
part of the event stream — it is what starting a run returned — and W4 will
express it as an HTTP status rather than a serialised value.

`Busy` is not an error. It is an ordinary thing that happens when someone
asks two questions at once, and the channels word it themselves rather than
core writing chat copy: Telegram replies, the web shows it inline.

## Testing

- **Accumulating a stream reproduces the whole text.** Drive `run_agent`'s
  update logic over the measured `strip_thinking` cases above and assert that
  applying the updates in order yields exactly `strip_thinking(full)` at
  every prefix. This is the property that makes the protocol correct.
- **The retraction is emitted.** Assert specifically that the `leak 29` case
  produces `Replace("")` — the event the old emptiness guard would have
  swallowed. Mutation check: restore the `is_empty` guard and watch this fail.
- **Telegram renders what it rendered before.** `render_events` over a
  delta stream produces the same sequence of rendered strings as the current
  code over a whole-text stream.
- **A second run on a busy conversation is `Busy`**, and the first run's
  history survives. Mutation check: drop the guard and watch the exchange
  vanish.
- The existing suite passes with only mechanical edits, as in the
  account-keying refactor: `progress.rs`'s tests construct `AgentEvent`
  values and must wrap them in `TextUpdate`. No assertion or expected value
  changes.

## Deferred, and why

- **`seq` numbers and `Last-Event-ID` replay.** The service-split design
  sketched both. W3b does not need them: the answer is written to the
  conversation when the run ends, so history is the source of truth and a
  dropped stream costs the animation rather than the answer. Replay would
  also have to reproduce `Replace` exactly or it would resurrect retracted
  reasoning — a sharp edge bought for no gain.
- **Cancellation.** `POST /runs/{id}/cancel` belongs with a client that has
  a stop button. W3b can add it.
- **Making `Tool` and `Notice` updates too.** Both are single discrete
  sentences that never grow, so a `TextUpdate` would be ceremony.
