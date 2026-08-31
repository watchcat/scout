# Core Keyed on Accounts — Design

## Purpose

`scout-core` is already keyed on accounts everywhere it stores anything. It is
not keyed on accounts everywhere it is *called*: four entry points still take a
Telegram user id or chat id and convert inside. That is why a browser cannot
drive the agent, and it is the whole of what W3 is waiting on.

This makes the entry points account-keyed. It is a refactor, not a feature.

## What this is not

**No behaviour changes.** The first draft of this section said every one of the
559 existing tests passes unmodified. Writing the plan showed that to be
unachievable and therefore useless as a criterion: changing a function
signature forces mechanical edits at roughly ten test sites that construct the
affected types. The rule that survives is narrower and actually checkable:

> **No test's assertions or expected values change.** Only identifiers and
> struct-literal field names change, and only where a signature changed.

A changed `assert_eq!` right-hand side is evidence the refactor altered
something it claimed not to, and is reported rather than adjusted. The plan
enumerates the forced edits up front, so an unexpected one is visible as
unexpected. The one new test named below adds coverage for a property that was
previously untested, and changes no existing one.

Nothing here adds an endpoint, a page, or a way for a browser to reach the
agent. W3 does that, and can only do it once this lands.

## The boundary

Core becomes account-keyed at every entry point. The Telegram adapter converts
at its own edge — once per update, before it calls anything else — and passes
account ids from there down.

Two things that sound like they should move, and the reason each does or does
not:

**`conversation_scope(chat_id, user_id)` moves to the adapter.** Deciding that
a chat id equal to the user id means `"direct"` and anything else means
`"telegram:<id>"` is a Telegram-shaped judgement. The *resulting string* stays
core vocabulary: `'direct'` is what the web client will share, exactly as the
`conversations.scope` column has said since phase one.

**`account_of` stays in core.** Resolving an identity to an account is a store
read, and the store is core's. What moves is the responsibility for calling it:
today core calls it three times, buried inside functions that took a Telegram
id; afterwards the adapter calls it once, at its boundary, and core never
converts anything again.

## The call sites

Only the parameters that change are listed; everything else keeps its shape.
`run_agent` still takes its event sink, its `conversation_id` and its prompt.

| function | takes today | takes after |
|---|---|---|
| `run_agent` | `user_id`, `chat_id` | `account_id`, `ReplyTo` |
| `resolve_conversation` | `user_id` | `account_id` |
| `over_daily_cap` | `user_id` | `account_id` |
| `reset` | `telegram_id` | `account_id` |
| `Core::log_request` | `telegram_id` | `account_id` |
| `account_of` | `telegram_id: i64` | `TelegramId` |

`Core::log_request` was missed when this document said "four call sites" and
was found while writing the plan. It matters more than its size suggests: the
daily cap counts the rows it writes, so a caller that cannot log a request
cannot be capped, and a web client that could not be capped would be a hole
rather than an omission.

Three neighbours deliberately stay Telegram-keyed. `note_display_name` and
`note_address` record facts only Telegram supplies. `is_founder` and
`is_admin` read `ALLOWED_TELEGRAM_USER_IDS`, which is Telegram ids by
definition — `Core::founder_account` asks the same question of an account by
looking up which Telegram ids it can prove.

## Splitting `chat_id`

The interesting part, and the reason this is a design rather than a rename.
One number does two unrelated jobs, and only separating them lets a caller that
is not Telegram exist at all.

### Where a reminder goes

`CreateReminderTool` writes `create_reminder(account_id, "telegram",
&chat_id.to_string(), …)`. The channel is hardcoded and the address is a
Telegram chat id.

This becomes an explicit `ReplyTo { channel, address }`, carried for the
duration of one run and handed to the tools that need it. The Telegram adapter
passes `("telegram", chat_id)`, which produces byte-identical rows to today.

`ReplyTo` lives in `scout-api`, beside `DueDelivery`, which already carries the
same `channel` and `address` pair. The two are the same vocabulary seen from
opposite ends — one says where an answer should go, the other where a due
message is going — and W4 puts both on a wire, where a type that names its own
channel can be logged and routed without the query beside it.

**The obvious simplification is wrong, and it is worth recording why.** It is
tempting to drop `channel` and `address` from `reminders` altogether and
resolve the destination at delivery time from the `deliveries` table, which
already maps an account to an address per channel. A reminder belongs to a
person, after all, and where to send it looks like a delivery-time question.

It is not, and the reason is sharper than the first draft of this document
claimed. That draft said resolving from `deliveries` would redirect group
reminders "into the creator's private chat". Checked against the code, it does
something worse: `note_sender` calls `note_chat` on *every* incoming message,
group or private, and `note_delivery` upserts on `(account_id, channel)`, so
`deliveries.address` is simply wherever that account last spoke. A reminder
made in one group could later be delivered to a different group, or to a DM,
depending on where its owner happened to talk next — non-deterministically,
and long after the fact.

In a group chat `chat_id` is the *group's* id, and a reminder made there is
delivered back there. The address is a property of where the reminder was
asked for, not of who asked, and `ReplyTo` says so.

What the browser passes is deliberately not decided here. A reminder created on
the web by someone with no Telegram identity has nowhere to go, and answering
that is W3's problem, taken with the rest of the web surface in front of it.

### The shown-flights dedup key

`ShownFlights` is an in-memory map that stops a flight already offered as
"option 2" being renumbered. It is keyed by `chat_id` today.

It becomes keyed by `conversation_id`, which distinguishes group chats exactly
as `chat_id` did, because the scope a conversation is opened under already
carries the chat.

One accepted difference: a long gap starts a new conversation, so the dedup
resets where today it persists until the process restarts. This is an
improvement rather than a tolerated regression — a fresh conversation
renumbering its options is less surprising than one that remembers flights
quoted last week, at prices that have since moved.

## `TelegramId(i64)`

A newtype wrapping the 24 Telegram-facing sites. Account ids stay plain `i64`.

The asymmetry is the point. Wrapping account ids too would touch 68 signatures
and 595 references across 19 files, and every DuckDB bind would need a `ToSql`
impl or a `.0` — a large mechanical diff landing on top of a refactor whose
value depends on being reviewable. Wrapping only the rarer of the two id spaces
blocks the direction that actually causes harm: a Telegram id reaching
something that expects an account id. The reverse is harmless, because an
account id has nowhere to go in the adapter.

The compiler then enforces what `account_of`'s doc comment currently only asks
for: *"a caller that forgets to convert gets a type that is still an `i64` but a
name that says which one it is."*

`TelegramId` lives in `scout-core`, not `scout-api`. Core keeps exactly one
Telegram concept and this names it: the founder allow-list is
`ALLOWED_TELEGRAM_USER_IDS` by configuration, so `is_founder` is inherently
Telegram-keyed and cannot become account-keyed without changing what a founder
is. Putting the type in core admits that honestly rather than pretending core
is free of Telegram and smuggling the ids through as bare integers.

## The founder check

`over_daily_cap` currently short-circuits on `core.is_founder(user_id)` before
touching the database. Account-keyed, the question becomes whether *this
account* belongs to a founder, answered by `store.telegram_ids(account_id)` and
testing each against the allow-list.

That costs one query, inside a blocking section that already runs
`requests_today`. It is also more correct than what it replaces: a founder who
links an email address and arrives from the browser is the same person and
stays exempt, where a Telegram-keyed check could not have said so.

## Testing

The suite is the specification here, so most of the work is proving nothing
moved.

- **The existing 559 tests pass unmodified.** Any diff to a test file is
  treated as a finding, not a fix.
- **A reminder made in a group still addresses the group.** This property is
  what rules out the `deliveries` simplification above, and it is currently
  untested — the only new test this design adds.
- **Mutation checks**, in the style the repository already uses: point
  `ReplyTo` at the wrong address and watch the group-reminder test go red;
  key `ShownFlights` on the account rather than the conversation and watch two
  group chats bleed options into each other.

## Deferred, and why

- **The SSE API and the browser client.** W3. This spec exists so that W3 has
  a core it can call.
- **What a browser passes as `ReplyTo`.** Requires deciding where a reminder
  goes for someone with no Telegram, which is a product question about the web
  surface rather than a refactor.
- **`AccountId(i64)`.** Rejected above on diff size, not on principle. If the
  `i64` confusion recurs on the account side, this is the answer.
- **Core owning Telegram's `/invite` grammar.** Recorded as deferred by the
  workspace phase and still deferred. It becomes urgent only when an endpoint
  is designed around `InviteCmd`, which W3 need not do.
