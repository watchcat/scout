# Scout — Service Split: Design

Date: 2026-08-18
Status: Approved

## Purpose

Scout is one process. `main.rs` builds every client, opens the DuckDB
file, spawns the scheduler and hands the whole thing to teloxide's
dispatcher. Telegram is not a front-end to Scout; Telegram *is* Scout's
only door, and its failures are Scout's failures.

On 2026-08-18 that stopped being theoretical. Telegram's API began
returning nothing for bot id `8849043058` at 20:17 UTC. Each start-up
called `get_me`, timed out after 17s, and panicked inside teloxide
(`dispatcher.rs:385`, "Couldn't prepare dispatching context"). Docker
restarted the container; it panicked again. Twenty-four times in twenty
minutes. The agent, the store and the reminder scheduler all died with
it, none of which had anything to do with Telegram being unwell.

This design splits Scout into a core service that owns the agent and the
data, and thin adapters that own one channel each. Telegram becomes the
first adapter; a web app becomes the second. A channel outage stops
being an outage of Scout.

## Scope

**In scope:**

- A `scout-core` service: the agent, the tools, the store, and an
  HTTP + SSE API
- A `scout-telegram` adapter: teloxide, progress rendering, chunking,
  flood control — and nothing else
- Identity that is not a Telegram user id, so a web user can exist
- Conversations that survive a restart and are shared across channels
- Splitting reminder delivery from reminder scheduling
- The three-phase order in which this is delivered

**Out of scope (deliberately):**

- The web app's interface, stack and login method — phase three, its own
  design
- Replacing DuckDB; see "Decisions"
- Any change to what the agent can *do*. No tool gains or loses a
  capability. The 412 existing tests are the contract.
- Multi-tenancy beyond what invite rounds already give us
- Horizontal scaling of core. One core process, as today.

## What the codebase already gives us

The seam is mostly cut already, which decides how much of this is
carpentry and how much is surgery.

**Almost nothing knows about Telegram.** Searching for `teloxide`
across `src/`:

| Module | References |
|---|---|
| `bot.rs` (2531 lines) | the adapter |
| `progress.rs` (395 lines) | 6 |
| `scheduler.rs` | 1 (`use teloxide::prelude::*`) |
| `agent.rs`, `store.rs`, `vision.rs`, `stats.rs`, `links.rs`, `draft.rs`, `text.rs` | none |
| all 18 files under `tools/` | none |

Roughly 19k of 22k lines are already transport-agnostic and move to
core unedited. The split is an extraction of `bot.rs` and
`progress.rs`, not an untangling of the core.

**The database is single-writer.** `Store::open` (`src/store.rs:282`)
is `Connection::open(path)` against an embedded DuckDB file behind one
`Arc<Mutex<Connection>>`. Two OS processes cannot hold it read-write.
Whatever else is true, exactly one service owns that file.

**Conversation history is in memory only.** `ChatSession`
(`src/bot.rs:66`) is a `Vec<LlmMessage>` plus a draft and a `last_seen`,
held in a `DashMap` on `App`. Nothing writes it to disk — a restart is
amnesia. Two front-ends cannot share a conversation that only exists
inside one process.

**Identity is a Telegram user id, in the schema.** Nine tables key on
`user_id BIGINT` and that value is the Telegram id: `purchases`,
`reminders`, `user_facts`, `request_log`, `users`, `user_chats`,
`members`, `waitlist`, `trips`. Three also carry a Telegram `chat_id`,
and `reminders.chat_id` is how the scheduler decides where to deliver.
A web user has no such id.

## Decisions

Five choices, each made deliberately, each cheap now and expensive
later.

**Identity is an internal account id.** New `accounts` and `identities`
tables; a Telegram user id becomes one identity row of kind
`telegram`, and a web login will be another of a different kind. The
nine tables migrate to `account_id`. This is the only option under
which someone without a Telegram account can use Scout, which is the
point of building a web app. It costs a migration across nine tables —
affordable at eight founders and one invited member, a serious
undertaking at ten thousand.

**One rolling conversation per account, shared across channels.**
Telegram and the web app are two windows onto the same thread: ask on a
phone, continue at a desk. The existing session TTL and `HISTORY_CAP`
behaviour is preserved exactly; it just becomes rows in `conversations`
and `messages` instead of a `DashMap` entry. Named, revisitable threads
are a superset we can add later without another schema change, and are
not built now.

> **Amended while planning phase one (2026-08-18): a conversation carries a
> scope, and a reminder keeps its own address.** Two places where the
> decision above, applied literally, would have changed behaviour nobody
> asked to change.
>
> Today `chats: DashMap<(chat_id, sender_id), ChatSession>` gives each group
> chat its own history. "One conversation per account" would merge a group's
> thread into the owner's private one. So `conversations` has a `scope`:
> `'direct'` covers the 1:1 Telegram chat and the web app — those two do
> share, which is the whole point — and a group is `'telegram:<chat_id>'`.
>
> Likewise, "`reminders.chat_id` becomes a row in `deliveries`" would send a
> reminder created in a group to the owner's DM, because `deliveries` holds
> one default address per channel. A reminder therefore keeps its own
> `channel` and `address`; `deliveries` holds the account default, used for
> announces.

**One protocol: HTTP with server-sent events.** A message is a POST;
the answer is an event stream. Browsers speak SSE natively, axum serves
it in a few lines, and it survives any proxy. Both adapters use the
same API, so there is no second streaming surface to keep in sync.
Cancellation is a separate POST rather than a bidirectional channel —
the only thing a client needs to say mid-run.

**DuckDB stays, and core owns it exclusively.** `store.rs` is already a
repository layer: every query sits behind a `Store` method, so changing
engines later is contained to one file. At this scale a single mutex is
invisible next to LLM latency — microseconds against seconds. Revisit
when a second writer process is genuinely wanted or the mutex shows up
in a trace. The split itself retires the sharper risk, since nothing
but core opens the file.

**Two services; the web app is static.** `scout-core` and
`scout-telegram` are the only Rust services. The web app is a static
bundle that talks to core's API directly, so there is no third backend
to write or deploy. If web login later needs server-held secrets, that
is a phase-three decision and it can be answered by adding endpoints to
core rather than a new service.

## Target architecture

### Workspace layout

A cargo workspace, not separate repositories. Both binaries depend on
one crate of wire types, so an event the adapter fails to handle is a
compile error rather than a production surprise.

```
scout/
  crates/
    scout-api/       lib  — request, response and SSE event types
    scout-core/      bin  — agent, tools, store, HTTP + SSE API
    scout-telegram/  bin  — teloxide, Live, chunking, flood control
  web/                    — static bundle (phase three)
```

`agent.rs`, `store.rs`, `tools/`, `vision.rs`, `links.rs`, `text.rs`,
`stats.rs` and `draft.rs` move into `scout-core` unedited. `bot.rs` and
`progress.rs` move into `scout-telegram`. `config.rs` divides: each
service reads only the variables it uses, so the Telegram adapter no
longer needs a Kagi key to boot.

### Core's API

```
POST /v1/conversations/current/messages   → text/event-stream
POST /v1/runs/{run_id}/cancel
GET  /v1/conversations/current            → history, for the web app
POST /v1/identities/telegram/resolve      → account_id, created on first admit
GET  /v1/members                          → ETag'd, for the adapter's gate
POST /v1/invites/rounds
POST /v1/invites/announce
POST /v1/invites/kick
GET  /v1/deliveries?channel=telegram
POST /v1/deliveries/{id}/ack
POST /v1/uploads                          → handle for an image
GET  /v1/purchases | /v1/trips | /v1/reminders
```

`current` is not an id but a resolution: the active conversation for the
authenticated account, created if the last one has aged past the session
TTL. The three read endpoints on the final line exist for the web app
and are built in phase three; everything above them is phase two.

Five event kinds on the stream, defined once in `scout-api`:

```
event: tool       {"seq":1,"text":"searching Kagi for a 4K monitor"}
event: thinking   {"seq":2,"delta":"comparing fares"}
event: token      {"seq":3,"delta":"The cheapest"}
event: done       {"seq":9,"message_id":42}
event: error      {"seq":9,"reason":"daily_cap","detail":"..."}
```

`seq` is monotonic within a run and is emitted as the SSE `id:` field,
which is what makes reconnection free (see "Failure handling").
`progress.rs::describe` (`src/progress.rs:229`) already renders a tool
call as human text; it moves to core and becomes the `tool` event's
payload, because every channel wants the same sentence.

### Who authorises what

Authorization moves to core. It has to: the web app needs the same
gate, and a daily cap enforced in two places will drift.

The adapter nonetheless keeps its member `DashSet`, for the reason it
was built with. The gate runs on *every* update, including messages
from people who were never invited, so anyone who finds the bot can
make Scout do work simply by typing at it. Moving that check across the
network would make it a request per stranger — worse than the database
read it was introduced to avoid. So:

- The adapter loads the set from `GET /v1/members` at start-up,
  refreshes it after any admin command it forwards, and re-polls every
  60 seconds with an ETag.
- Core remains authoritative. `/start <code>` claims always go to core:
  they are rare, and `claim_seat` is an atomic check-and-insert that
  must not be cached or it will oversell a round.
- The daily cap lives only in core and is checked once per run.

Between services, a shared bearer token on the private compose network.
The adapter is trusted to assert "this is Telegram user 12345" — the
same trust already placed in it today.

### What a message does

```
Telegram update
  → adapter: gate check against the cached member set
  → adapter: telegram id → account id (cached)
  → POST /v1/conversations/current/messages
      → core: daily cap, request log, load history, build agent, stream
      ← event: tool     {"seq":1,"text":"searching Kagi for …"}
      ← event: token    {"seq":2,"delta":"The cheapest"}
      ← event: done     {"seq":9,"message_id":42}
  → adapter: Live renders each event, paced against Telegram's limits
  → adapter: final send, record reply id so a later 👍 resolves
```

Pacing stays entirely adapter-side: `edit_interval`
(`src/progress.rs:52`), the process-wide `streams` counter, and
`quiet_until` for flood control. Core emits events as fast as the model
produces them and knows nothing about rate limits. That is the right
boundary — Telegram's ceiling is per bot token, which is a fact about
Telegram, not about answering a question.

## The scheduler splits in two

Deciding that a reminder is due is core's work; delivering it belongs to
a channel. The adapter polls rather than core pushing:

```
GET  /v1/deliveries?channel=telegram   → due items, each with an address
POST /v1/deliveries/{id}/ack
```

Polling because it survives an adapter restart with no retry queue to
build, and because it extends to web push in phase three without core
learning a new transport. `reminders.chat_id` stops being identity and
becomes a row in `deliveries`.

The invite announce divides the same way. Core chooses the recipients
and records the outcome — `mark_invited` on success, `forget_waitlist`
on a recipient who has blocked the bot. The adapter does the sending;
the `broadcast` loop and `Delivered::{Ok, Gone, Failed}` are pure
Telegram and stay where they are. Core is simply told which recipients
came back `Gone`.

## Photos

The adapter owns Telegram's file API: it calls `GetFile`, POSTs the
bytes to `/v1/uploads`, and passes core the returned handle. Core runs
the vision call. `pending_draft` moves into core's conversation state
alongside history, so a drafted photo survives a core restart — which
it does not today.

## Failure handling

**Telegram is unwell.** The adapter retries with backoff; core is
untouched and keeps serving the web app. Neither service panics on a
start-up dependency failure. Concretely, the `get_me` call at
`src/main.rs:129` and teloxide's dispatcher preparation are wrapped so
a timeout is a logged warning and a retry. That one change is the
difference between the 2026-08-18 incident being twenty-four crash
loops and being a line in the log.

**Core is unwell.** The adapter says so plainly in chat and keeps
polling. It does not exit — an adapter that dies when its dependency
blinks has merely moved the original problem one layer outward.

**The stream drops mid-run.** The run continues server-side. Because
every event carries an `id:`, reconnection uses the standard
`Last-Event-ID` request header and core replays from a short in-memory
buffer; there is no bespoke resume protocol. A run whose client never
comes back still finishes and still writes its answer to `messages`, so
the work is not lost.

**A request is refused.** Gate and cap rejections are typed reasons on
the `error` event, not prose. The adapter renders `INVITE_ONLY`; the web
app renders something suited to a browser; both from one signal.

## Testing

The split makes two things testable that currently are not.

**Streaming and pacing.** `Live` (`src/progress.rs:106`) can be driven
from a synthetic event stream with no Telegram present, so the flood
control and `edit_interval` logic gets direct tests for the first time.

**The contract.** Both binaries compile against `scout-api`; an event
kind the adapter does not handle fails the build.

Beyond that: core keeps the existing test suite — 412 passing and 3
ignored as of this writing — which moves with the modules and must stay
green — they are the main safety net for the
whole exercise. API-level tests drive axum through
`tower::ServiceExt::oneshot` rather than binding a socket. One
migration test opens a pre-migration fixture database and asserts that
every legacy `user_id` maps to exactly one account, with no orphans in
any of the nine tables.

## Delivery, in three phases

Each phase ships on its own and is useful on its own.

**Phase one — accounts and persisted conversations.** Still a single
process. Nothing changes in how Scout answers, with one deliberate
exception: a conversation now survives a restart instead of being lost
with the process. Add `accounts`, `identities`,
`conversations`, `messages` and `deliveries`; migrate the nine tables to
`account_id`; move history out of the `DashMap` and into the store.
This carries the only irreversible data change, so it travels alone
rather than tangled with a process split.

**Phase two — the split.** Extract `scout-core` and `scout-telegram`
behind the API above. Telegram behaviour is identical from the outside;
the observable difference is that a Telegram outage no longer stops the
agent or the scheduler.

**Phase three — the web app.** A static front-end against the same API,
plus web login as a second identity kind. Its own design document,
where the interface questions belong.

> **Amended before starting phase two (2026-08-20): two is split in half.**
> Reading the code first showed the coupling is far thinner than assumed —
> `Live`'s whole surface is four methods, and inside `run_agent` there are
> five calls to it: a tool description, the answer so far, thinking twice,
> and the wrap-up notice. Everything else in that function is already
> transport-agnostic.
>
> That makes the semantic change separable from the transport one:
>
> **2a — the event protocol, one process.** `run_agent` stops holding a
> `Live` and emits events into an in-process channel; the adapter consumes
> them and renders. One binary, behaviour unchanged, but the agent no longer
> owns the renderer — which is the part that could turn out to be wrong.
>
> **2b — the workspace and the wire.** Crates, axum, SSE, two containers.
> The channel becomes a socket and little else changes.
>
> The event carries the **cumulative** text rather than a delta, because
> that is what `Live::show` already takes and `Live` does its own diffing.
> Sending whole snapshots over a socket is wasteful, so 2b may switch to
> deltas — but doing it in 2a would change behaviour while claiming not to.
>
> One deliberate consequence: with a channel between them, the agent loop no
> longer blocks on Telegram's rate limiter. That is a change, and a wanted
> one — it is exactly what a split has to be true for.

Phase one precedes phase two because the API is conversation-scoped:
`/v1/conversations/current/messages` cannot be specified before
accounts and conversations exist.

## Migration

New tables: `accounts`, `identities`, `conversations`, `messages`,
`deliveries`. The backfill takes the distinct `user_id` across all nine
tables, mints one account for each with an `identities` row of kind
`telegram`, then rewrites the foreign keys. It runs as an ordinary
migration at core start-up, so there is no separate tool to build.

Two cautions belong in the implementation plan:

- **Copy the `.duckdb` file before running it.** The backfill cannot be
  undone.
- **Copy it from inside the container, never from the host.** The
  `duckdb` crate bundles its engine, and the locked `duckdb 1.10501.0`
  is **DuckDB v1.5.1** — verified by running `SELECT version()` through
  the crate, and the deployed image was built from this same lockfile.
  The Python DuckDB on the development machine is **1.4.5, which is
  older**. It cannot be relied on to open the file at all and must
  never write to it. Take the copy with `docker compose exec`, or with
  core stopped. That file holds the real purchase history and the live
  reminders.

## Deferred to phase three

Recorded here so they are not mistaken for oversights: how a web user
logs in; whether the web app offers named conversations; what the web
interface looks like; whether reminders reach a browser by push or by
email. None of them change anything above.
