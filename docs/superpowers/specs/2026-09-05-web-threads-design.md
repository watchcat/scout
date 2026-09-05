# Threads in the Browser — Design

## Purpose

The web chat shows one conversation: the newest `direct` thread, the one a
1:1 Telegram chat also continues. "New thread" starts a fresh one and the old
one becomes unreachable, though it is still in the database. This gives the
browser a list: several threads, each keeping its own context, switchable at
will, gone after two days of silence unless pinned.

## Decisions taken

These were settled in conversation and the rest of the document follows from
them.

- **Telegram continues the thread you last used on the web.** One "current"
  thread per account, shared by both channels and the mirror.
- **The web is explicit; Telegram keeps the 10-minute rule.** In the browser
  the thread you opened is the thread you are in. On Telegram the idle check
  stays, and when it decides "new topic" it starts a new thread, which then
  appears in the browser's list and is current.
- **Titles are the first message, trimmed**, with a button that asks the
  model for a better one.
- **A sidebar** on wide screens, a drawer on a phone.
- **Current means most recently touched.** Opening a thread bumps its
  `updated_at`, which is what `latest_conversation` already orders by. No
  pointer column, no second timestamp. The list is ordered by last use, so
  opening a thread moves it to the top.

## Data

Migration step 7 adds two columns to `conversations`, and step 8 makes the
pin non-null:

```sql
-- step 7
ALTER TABLE conversations ADD COLUMN IF NOT EXISTS title TEXT;
ALTER TABLE conversations ADD COLUMN IF NOT EXISTS pinned BOOLEAN;
UPDATE conversations SET pinned = false WHERE pinned IS NULL;
ALTER TABLE conversations ALTER COLUMN pinned SET DEFAULT false;

-- step 8
ALTER TABLE conversations ALTER COLUMN pinned SET NOT NULL;
```

The NOT NULL is a step of its own because DuckDB refuses `SET NOT NULL` in a
transaction that has already touched the table's rows — the `ADD COLUMN` and
the backfill in step 7 both count — and `apply_steps` gives each step its own
transaction.

`title` is null until the thread's first answer lands. Then it is the
person's own first message, whitespace-collapsed, cut to 40 characters on a
character boundary with `…` appended when cut — never the prompt the model
saw, which on Telegram can carry a system note the person never wrote. It is
set in exactly one place — the end of `run_agent`, via `title_if_missing` —
so a thread started from Telegram gets a name too. A rename overwrites it
and is never overwritten by `title_if_missing`, because that only writes
when the column is null.

`pinned` is "permanent". Nothing else changes about a pinned thread.

## Lifecycle

**Expiry.** `Core::run_maintenance` gains an hourly step beside the
login-token prune: `Store::expire_conversations(48 * 3600)` deletes every
unpinned conversation with `updated_at` older than that, and its messages,
in one transaction, returning the count for the log. It applies to every
scope. A group thread cannot be pinned from anywhere; a group silent for two
days would have been judged a new topic by the 10-minute rule in any case.

**No current thread.** If the current thread expires or is deleted, both
channels do what they do today when there is none: the next message starts a
fresh one. On the web the list is either empty or shows the newest remaining
thread as current.

**Deletion.** Allowed on any thread, pinned or not, behind a confirm dialog.

**Ownership.** Every store operation on a thread takes the account id and
matches on it in the `WHERE`. Someone else's thread id, or one that no
longer exists, is not-found; nothing distinguishes the two to the caller.

**No cap** on thread count. With a two-day expiry and `HISTORY_CAP` (20)
messages per thread, holding many is a choice made by pinning.

## Core

In `scout-api`, beside `Turn`:

```rust
pub struct Thread {
    pub id: i64,
    pub title: Option<String>,
    pub pinned: bool,
    pub updated_at: String,   // RFC 3339, UTC
    pub current: bool,
}
```

In `session.rs`, all account-keyed, all through `blocking`:

| function | does |
|---|---|
| `threads(core, account)` | every `direct` conversation, pinned first, then by `updated_at` desc; `current` set on the row `latest_direct` would return |
| `open_thread(core, account, id)` | verifies ownership, bumps `updated_at`, returns the transcript |
| `reset` | unchanged; this is "new thread" |
| `rename(core, account, id, title)` | trims, refuses empty, cuts at 80 characters |
| `set_pinned(core, account, id, bool)` | |
| `delete_thread(core, account, id)` | the conversation and its messages |
| `suggest_title(core, account, id)` | one tool-less model call over the transcript, "a title of at most five words"; stores and returns it |
| `title_if_missing(core, id, source)` | called by `run_agent` after a successful save, only when `RunContext::title_source` is `Some`; writes only when `title` is null |

The store gains matching methods plus `expire_conversations`. `latest_direct`
is unchanged and remains the single definition of "current".

`RunContext` gains `title_source: Option<String>`, set by each channel to
the person's own words for naming a thread, or `None` when this run has
none worth naming it after: Telegram sets it to the plain message or the
bare photo draft, never the augmented prompt the model is asked; a reaction
sets it to `None`, since a reaction has no words of its own; the web route
sets it to the message body, which is the prompt there with nothing
appended. `run_agent` is otherwise untouched: it already reads and writes
the conversation it is handed.

## Web routes

On the authenticated router with the existing CSRF header check on every
POST:

| route | does |
|---|---|
| `GET /chat/threads` | the list, JSON `Vec<Thread>` |
| `POST /chat/threads` | new thread; returns its `Thread`. `/chat/reset` stays as an alias |
| `POST /chat/threads/{id}/open` | switch; returns the transcript |
| `POST /chat/threads/{id}/rename` | body `{"title": "…"}` |
| `POST /chat/threads/{id}/pin` | body `{"pinned": true}` |
| `POST /chat/threads/{id}/delete` | |
| `POST /chat/threads/{id}/title` | auto-rename; returns `{"title": "…"}` |

`POST /chat/messages` gains `"thread": <id>` in its body. The server
verifies ownership and runs on that thread rather than on whatever is
newest. This closes a race: Telegram may have started a new thread while the
page sat open, and a web message would otherwise land in the phone's thread.
The run bumps `updated_at`, so the thread becomes current as a side effect.

`GET /chat/history` is unchanged and returns the current thread, so the page
still loads with one call before fetching the list.

Any thread route given an id the account does not own, or that no longer
exists, answers 404. `send_message` answers 404 before anything runs.

## The page

`chat.html` becomes a two-column grid above 720px — sidebar, then the
existing column — and below that the sidebar is a drawer opened by a menu
button in the header. Styling stays inline, matching the page.

The sidebar holds "New thread" at the top and the list beneath. A row shows
the title, or "New thread" in muted text when there is none yet; a relative
time; and a pin mark when pinned. An unpinned row older than 36 hours shows
"expires in Nh" in place of the time. The current row is highlighted.
Controls appear on hover and on the current row: pin toggle, rename (the
title becomes an input; Enter saves, Escape cancels), auto-rename (a small
button beside rename), delete (confirm dialog).

The list is fetched on load, after every run ends, when the tab regains
focus, and after any thread action. Switching clears the turns, renders the
returned transcript, and sets the composer's thread id. A run in progress is
not interrupted by a switch: its stream keeps rendering into the element of
the thread it belongs to, and switching back shows the finished answer via
the history call.

New client logic lives in `chat.js` as exported pure functions where it can
be — list ordering, the "expires in" wording, the send body — so it is
tested like `applyUpdate` and `parseFrame` are.

## The mirror

The toggle stays account-level and keeps following the current thread,
because it reads `current_thread`, which reads `latest_direct`. On a switch
with the mirror on, one divider goes to the phone naming the thread —
`── cheapest OneBlade cartridges ──` — and nothing else. Turning the toggle
on still backfills the current thread as it does today.

## Errors

- A 404 from any thread route makes the page refresh the list; if the
  vanished thread was current, it shows the newest remaining one or the
  empty state.
- A message sent into a thread that expired between load and send gets the
  404; the page says the thread is gone and offers a new one.
- A failed auto-rename leaves the title as it was and uses the existing
  notice line.
- An expiry failure is logged and retried on the next hour, like the
  login-token prune.

## Testing

**Store.** Step 7 applies to an existing database and to a fresh one. Expiry
deletes an unpinned 49-hour-old thread and its messages, keeps a pinned one
and a 47-hour one, and reports the count. Every thread operation given
another account's id reports not-found and changes nothing. Deleting the
current thread makes the newest remaining one what `latest_conversation`
returns.

**Session.** `threads` orders pinned first, then by last use, and marks
exactly one row current. `open_thread` makes that thread what
`latest_direct` returns. `title_if_missing` writes a 40-character title once
and never over a rename. `suggest_title` is tested through the same seam as
`continues_previous`.

**Web routes.** With `seed_exchange_for_tests`: list, open, rename, pin,
delete, 404 on a foreign id, and a `send_message` naming a thread the
account does not own is refused before anything runs.

**Client.** The pure functions above, beside the existing `chat.js` tests.

**Source assertions**, the repository's habit: `run_agent` calls
`title_if_missing`; `run_maintenance` calls `expire_conversations`.

## Out of scope

Threads for Telegram groups in the browser; a `/threads` command in
Telegram; search across threads; exporting a thread; any change to
`HISTORY_CAP`.
