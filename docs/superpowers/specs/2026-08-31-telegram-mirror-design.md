# The Browser Thread, On Your Phone — Design

## Purpose

The browser and a 1:1 Telegram chat already share one conversation. `scope.rs`
maps a private chat to `"direct"` and `/chat` resolves the same scope, so the
*context* carries across devices today: ask on the laptop, open Telegram, and
the bot already knows what you asked.

What does not carry is the conversation itself. The Telegram chat shows
nothing, so picking the thread up on a phone means continuing a conversation
you cannot see. This puts the messages there.

This is about continuity, not notification. Nothing here needs to be live.

## What you get

A Telegram toggle in the `/chat` header. Turn it on and the current thread is
sent to your Telegram chat — all of it, then each new exchange as it
completes. Turn it off and it stops.

An exchange arrives as two messages:

```
── message 1 ──
> Find me cheapest Philips OneBlade cartridge

── message 2 ──
EUR 24.24 delivered, bol.com (2 blades, EUR 12.12/blade)
https://www.bol.com/nl/nl/p/philips-oneblade-qp220-50/
```

Two messages rather than one so that a 👍 on the answer resolves to the answer
alone. `handle_reaction` feeds the reacted-to text back to the model as "this
earlier reply of yours"; a blob containing your own question would be a worse
prompt than the answer by itself.

The `>` is a **literal character in plain text, not a MarkdownV2 blockquote.**
The bot sends plain text everywhere except two admin paths, and for good
reason: an answer is model output full of `*`, `_`, `[` and `.`, every one of
which MarkdownV2 requires escaped. Getting that wrong turns a price list into
a parse error. A literal `>` reads fine and cannot fail.

## Architecture: a table, not a channel

Two constraints decide this.

**The dependency runs `scout-telegram → scout-web → scout-core`,** so the web
route cannot reach `Bot`.

**No stored message has a stable identity.** `replace_messages` deletes and
reinserts the whole conversation on every save, `position` is renumbered from
zero each time, and `trim_history` drops from the front. A "mirrored up to
here" pointer would reference a row that no longer means what it meant.

A durable outbox resolves both:

```sql
CREATE TABLE IF NOT EXISTS outbox (
    id              BIGINT PRIMARY KEY DEFAULT nextval('outbox_id_seq'),
    account_id      BIGINT NOT NULL,
    channel         TEXT NOT NULL,
    address         TEXT NOT NULL,
    body            TEXT NOT NULL,
    turn_key        TEXT NOT NULL,
    attempts        BIGINT NOT NULL DEFAULT 0,
    created_at      TIMESTAMP NOT NULL DEFAULT current_timestamp,
    sent_at         TIMESTAMP,
    UNIQUE (account_id, turn_key)
);
```

The web side enqueues, the Telegram side drains. Neither calls the other; they
meet at a table. That is the same shape `reminders` already uses, including
the property that matters most — **a row that is not marked sent is retried**,
so a failed send is a delay rather than a lost message.

`turn_key` is what replaces the impossible watermark. It is
`sha256(conversation_id || role || text)`, hex-encoded, and the `UNIQUE`
constraint makes enqueueing idempotent. The question stops being "how far have
I got through a table that renumbers itself" and becomes "have I already sent
this turn", which stays answerable no matter how often the rows underneath are
rewritten.

That single change collapses three cases into one. Backfill, live mirroring,
and turning the toggle off and on again are all just *enqueue these turns;
duplicates are no-ops*.

`sha2` is already a workspace dependency (`scout-web` signs cookies with it)
and moves into `scout-core`'s manifest. It is not chosen for cryptographic
strength — it is chosen because `DefaultHasher` is explicitly not stable
across Rust releases, and a key that changes under a toolchain upgrade would
silently re-send every thread.

### The alternative, and why not

An in-memory channel from `Core` to the Telegram task is less code, and it is
what live streaming would need. It loses everything on restart. A fifteen-turn
backfill interrupted by a deploy would be half-delivered with no record of
where it stopped, and this repository spent 31 August establishing that
deploys land in the middle of things.

## The echo, which is the trap

The browser and Telegram share conversation `"direct"`. So "backfill the whole
thread" means sending Telegram its own messages back — half that thread
originated there, and nothing in `messages` records where a turn came from.
There is no metadata column and no stable identity to hang one on.

The ledger answers it. **The Telegram channel writes its own exchanges into
the outbox already marked sent** — same `turn_key`, `sent_at` set at insert,
no message dispatched. Backfill's idempotent enqueue then skips them for free,
because "already in the ledger" and "already delivered" are the same fact.

No origin column, no new concept: the channel that handled a turn records that
it needs no delivery. This is the part most likely to be got wrong, and it is
the part with the most direct test.

## Where the calls live

Core owns the table and the idempotence, and knows nothing about policy. A new
`scout_core::mirror` module offers `is_enabled`, `enable`, `disable`,
`enqueue(account_id, address, turns, delivered)`, `pending(channel, limit)`,
`sent(id)` and `failed(id)`.

**Each channel decides for itself what its own turns mean**, which is the same
division `ReplyTo` already draws:

- `/chat`'s run handler, on a successful run, calls `enqueue(.., delivered:
  false)` — but only when `is_enabled` and only when `reply_to_for` yields an
  address.
- `bot.rs`'s handlers, on a successful run, call `enqueue(.., delivered:
  true)`. Telegram already has those messages; recording them is what stops
  the backfill echoing them back.

`run_agent` is not the place for either. It is shared by both channels and has
no business knowing which one it is serving — that is exactly what `RunContext`
exists to carry, and it carries an address for reminders, not a policy.

Enabling the toggle enqueues the current thread's turns and returns. It writes
rows; it does not send anything. The drain does the sending, so a twenty-row
backfill is a fast database write and a slow background delivery, not a slow
HTTP request.

## The setting

`accounts` is deliberately almost empty — its own comment says everything
knowable belongs to an identity or to data, not there — and `store.rs`
contains no `ALTER TABLE ... ADD COLUMN` anywhere. So a table:

```sql
CREATE TABLE IF NOT EXISTS mirrored_accounts (
    account_id BIGINT PRIMARY KEY,
    enabled_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
```

**Presence is the setting.** A row means on, no row means off. There is no
boolean that can disagree with itself, and the timestamp is something the
backfill wants anyway.

## The control

A Telegram glyph toggle in the `/chat` header, beside "New thread". Not in the
composer: that design's own rule is that a composer holds one action, and a
second control beside Send is a chance to do the wrong thing by aiming badly.

It is **absent entirely when the account has no Telegram identity**, which
`reply_to_for` already determines — the same call that decides whether a run
may promise a reminder. A control that cannot work is a promise the page
cannot keep, and `/account` is where you attach a way in. If you link Telegram
later, the toggle appears.

`POST /chat/mirror` carries the CSRF header, like `/chat/messages` and
`/chat/reset` (replaced by `POST /chat/threads` on 2026-09-05).

## Draining

A loop in `scout-telegram` beside the reminder scheduler, woken by a
`tokio::sync::Notify` that the enqueue side pings, with the existing tick kept
as a floor. The notify is what makes it prompt; the floor is what makes a
missed signal a delay rather than a lost mirror.

Rows go oldest-first by id. Two rules matter more than they look:

- **A failed row blocks its own account, not the queue.** Skipping past it
  would land a later turn ahead of an earlier one, and a thread out of order
  is worse than a thread that is late.
- **Attempts are counted, and give up at five.** The reminder path can retry
  forever safely because dates bound it. An outbox row has no such bound: if
  you block the bot, an uncapped retry loops until a human notices. Five
  attempts, then abandon the row and log it.

Sending reuses `split_message(text, TELEGRAM_LIMIT)` so a long answer chunks
the way every other answer does.

A full backfill is bounded by `HISTORY_CAP` (20 messages), and `turns_of`
drops every message carrying a tool call — so twenty stored messages is *at
most* twenty mirrored ones and in practice far fewer, because one research
turn spends most of its messages on tool traffic. Twenty-odd seconds at a safe
pace, worst case.

## New thread

`/chat/reset` (replaced by `POST /chat/threads` on 2026-09-05) enqueues a
divider when mirroring is on. Without one, scrolling back through Telegram
runs two unrelated conversations together with no seam, which works against
the whole point.

The divider is a row like any other, with its own `turn_key` derived from the
conversation it closes, so it cannot be sent twice.

## Testing

- **`turn_key` is stable and distinguishing.** Same conversation, role and
  text gives the same key; a different role or a different conversation gives
  a different one. This is what the whole idempotence argument rests on.
- **Enqueueing the same turn twice inserts one row**, and enqueueing a turn
  the Telegram channel already recorded inserts nothing new and dispatches
  nothing. That is the echo guarantee, stated directly.
- **A turn recorded by Telegram is never dispatched** — `sent_at` is set at
  insert, so the drain never sees it.
- **The drain sends in order and stops an account at its first failure**,
  proven with a recording fake rather than a bot. `progress.rs`'s `Renderer`
  is the precedent: a trait, a `Recorder` impl, no token and no network.
- **A row is abandoned after five attempts** rather than retried forever.
- **The toggle is absent without a Telegram identity**, and `POST
  /chat/mirror` without the CSRF header is refused.

The drain loop's *wiring* — that the notify wakes it — needs a live process
and gets no test, in line with how `run_agent`'s branches are handled: the
pure decisions get extracted and tested, and the wiring is stated plainly as
untested rather than covered by something that only looks like a test.

## Not building

- **Live mirroring.** The purpose is continuity, not watching a run from a
  phone. Streaming would need a second `render_events` driving a `Live`
  against the same flood-control budget the bot already spends, and buys
  nothing here.
- **Telegram → browser, live.** Reloading `/chat` already shows Telegram's
  turns, because the thread is shared. Pushing them into an open page needs
  the SSE stream to carry events nobody's run produced.
- **A second setting table.** `mirrored_accounts` holds one fact. A generic
  key-value settings table is the thing to build when there is a second
  setting, not before.

## Deferred, and why

- **Threads and Telegram topics.** Private-chat topics are real (Bot API 9.4,
  February 2026), but they depend on a rework of the conversation model —
  today `"direct"` is *one* thread chosen by recency and an LLM continuation
  check. They also carry live external risk: `teloxide-core` 0.13 predates the
  feature and has no `has_topics_enabled`, and
  [tdlib/telegram-bot-api#847](https://github.com/tdlib/telegram-bot-api/issues/847)
  reports `400: message thread not found` on outbound private-topic sends
  after the Bot API 10.0 rollout. That wants proving against the live bot
  before anything is built on it. Its own spec.
- **The web thread sidebar.** Same dependency on the conversation model, plus
  thread naming, which is a summarisation problem rather than a UI one.
- **Raising `HISTORY_CAP` past 20.** Probably justified — the cap counts
  messages including tool calls and results, so one price comparison can evict
  the previous question — but it multiplies the prompt on every request, and
  the number should come from measuring a real research thread rather than
  from a guess.

**The outbox is unaffected by all three.** It is thread-agnostic: adding
`message_thread_id` later is a column, and the toggle, backfill, echo
prevention and retry logic do not move.
