# A Trip Knows Its Chat — Design

## Purpose

The trip view shipped read-only plus one decision: you can see the itinerary
and choose between flights already parked against a segment. Everything else
— adding a leg, removing one, asking a follow-up — means leaving the tab,
finding the right thread, and describing in prose a trip that is on screen.

This gives the trip three things it lacks: a link back to the chat that
created it, the ability to edit its own legs, and a composer that talks to
that chat without leaving the view.

## Not building

**Live re-search of a leg** — "check for more options and current prices" —
is deliberately out. It needs `search_flights` against Duffel or Ignav, which
is billed per query, and `trips.rs`'s own module doc draws the line: a channel
"cannot reach `Store`, invent candidates, or handle live offers: those remain
flight-agent responsibilities." Letting the web client spend money directly is
a decision that deserves its own spec, not a bullet inside this one. Until
then, typing a message is the route to fresh prices, which is part of why the
composer matters.

**Deleting a trip from the trip view.** `delete_trip` exists and the button
would be easy. Nobody asked for it, and a destructive control beside a
newly-added remove-leg control is how the wrong one gets pressed.

## The problem the lifetimes create

Conversations expire on their own. `Core::THREAD_IDLE_SECS` is `48 * 3600`,
and `expire_conversations` runs hourly, deleting any unpinned thread untouched
for two days. Trips do not expire at all: "Japan in spring" is built over
weeks.

So "deleting a chat deletes its trip", implemented literally, is not a feature
— it is a timer. Every trip would vanish two days after its chat went quiet,
with no button pressed and nothing to undo.

**The rule is therefore: destruction is explicit, expiry is not.**

- `delete_conversation` — someone pressed Delete — takes the trip with it.
- `expire_conversations` — a timer fired — sets the link to `NULL` and leaves
  the trip alone.

One line separates a tidy-up from data loss, and it lives in the sweep.

## The link

One nullable column, `trips.conversation_id BIGINT`.

Nullable is the design, not an oversight. `NULL` means orphaned, which is an
ordinary state a trip reaches by outliving its chat — not an error, not a
repair case.

### Who owns a trip

Trips are addressed by name, `UNIQUE (account_id, name_key)`, and created
through `upsert_trip`. Saying "add Rome to my Atlantic loop" in a fresh thread
extends the existing trip. So a trip has one creator and potentially many
chats that touched it.

Ownership is single, because the composer needs exactly one place to send to:

- The chat that **created** the trip owns it.
- If `conversation_id IS NULL`, the next chat to touch the trip **adopts** it.

`upsert_trip` sets `conversation_id` only where it is currently `NULL`. That
is one `WHERE` clause and no second code path: a live owner is never
displaced, and an orphan heals as soon as the traveller talks about it again.

The alternative — last toucher wins — was rejected because deleting your most
recent chat would destroy a trip whose original planning thread is still
sitting there.

### Threading the id

The trip tools carry `account_id` today — `FinaliseTripTool`
(`tools/trips.rs:515`), `AddTripSegmentTool` (`770`), `AddTripOptionTool`
(`919`), and the others that write — and gain `conversation_id` beside it,
from the run that built them. Only the tools that *write* need it; the
read-only ones (`ShowTripTool`) do not. This is the bulk of the diff and none
of it is interesting: the same value already flows to these structs, one field
wider.

## Editing legs

Two routes beside the existing `POST /chat/trips/choice`, both gated by
`admitted_account`, both CSRF-checked, both following
`choose_candidate_for_account`: resolve the trip by name against the proved
account, then act, **in one lock acquisition**. A web route that resolves a
trip it owns and then writes in a second acquisition is a race the store
already refuses to lose elsewhere.

### `POST /chat/trips/segment` — add a leg

Takes trip name, position, origin, destination, date. Reuses `iata()` and
`calendar_date()`, the validators the model's tools already use, so the web
and the model reject exactly the same inputs and cannot disagree about what a
valid leg is.

### `DELETE /chat/trips/segment` — remove a leg

Carries an `ExpectedSegment`.

This is not defensive padding. `drop_segment` **renumbers**: deleting position
0 shifts position 1 down to 0, and its candidates with it
(`store.rs:2651-2660`). A browser tab holding a trip rendered thirty seconds
ago is one concurrent edit away from asking to delete "leg 1" and destroying a
leg that is no longer the one it drew.

`add_candidate` already solved this and says why in its own comment: the
caller passes what it validated, and the store re-checks it inside the lock
that guards the write. Remove-leg does the same. A mismatch is not an error
but an outcome — the same shape as `Selection::CandidateNotFound` — worded to
the reader as "that leg changed, reload", because in a browser tab it is an
ordinary race and not a fault.

## The composer

The Trips view is a tab in the chat page (`chat.html:449`), not a separate
page, so the composer, the SSE reader and the streaming code are already
present. Nothing about sending needs to be built twice.

On the Trips tab the composer retargets, with a line above it naming where the
message will land:

```
↩ to "Cheap flights in October"
┌─────────────────────────────────┐ ┌─┐
│ Ask about Atlantic loop         │ │↑│
└─────────────────────────────────┘ └─┘
```

Three cases, because a trip's owner is not always somewhere the web can post:

| Owner | Line reads | Sending |
|---|---|---|
| A `direct` thread | `↩ to "<title>"` | posts there |
| `NULL` (orphaned) | `↩ to a new chat` | starts a thread, which adopts the trip |
| A Telegram group | `Planned in a Telegram group` | offers a new `direct` thread that does **not** take ownership |

Web chat and 1:1 Telegram share the `direct` scope (`scope.rs:11`), so a trip
planned by texting the bot is owned by the very thread the web view shows.
Only group chats (`telegram:<id>`) are elsewhere, and a group-owned trip keeps
its owner so the group's own cascade stays intact.

**Sending switches to the Chat tab and streams there.** When the run ends, the
Trips tab reloads so the itinerary reflects whatever changed. The alternative
— streaming into a panel on the Trips tab — means a second surface rendering
model output, two things to keep in sync, and duplicated streaming code. With
live re-pricing deferred, a typed message is the only path to fresh prices, so
the reply is prose about flights and belongs in the transcript where prose
lives.

## Migration

`conversation_id` is a new column on an existing table, so it needs both:

- the column in `MIGRATIONS`' `CREATE TABLE`, for databases created fresh, and
- an `ALTER TABLE` in a numbered step, for databases that already exist.

This is the pattern `conversations.title` used in step 7, and the reason that
table's own comment explains its column order. The new step is **9**;
`steps()` currently ends at 8.

`apply_steps` takes an automatic snapshot before the first step
(`store.rs:803-819`), verified working in production — the 6→8 migration on
2026-09-05 left `scout-2026-09-05T142740Z-migration-v8.duckdb` on disk. That
protects against a bad migration.

It does not protect against losing the disk. The off-site backup CronJob has
never completed successfully, so every copy — nightly and pre-migration alike
— sits on the same volume as the database it protects. Fixing the R2
credentials before running this migration is the recommendation; it is not a
blocker and it is not something this spec can do, because only the account
holder can mint the key.

## Testing

The point of these is to catch what a diff cannot show.

**The sweep detaches and does not delete.** Seed a trip owned by a thread, age
the thread past `THREAD_IDLE_SECS`, run `expire_conversations`, and assert the
trip is still there with `conversation_id IS NULL`. This is the test that
stands between the feature and silent data loss, and it must fail loudly if
the cascade is ever moved into the sweep.

**Explicit delete does cascade**, in the same transaction — assert both the
conversation and its trip are gone, and that a trip owned by a *different*
thread survives.

**Adoption only fills a hole.** A second chat touching a trip that already has
a live owner must not change `conversation_id`. Assert both directions: the
owned trip keeps its owner, the orphaned one gets the new chat.

**A stale remove hits nothing.** Render a trip, drop segment 0 behind the
browser's back, then send the original delete for segment 1 with its
`ExpectedSegment`. Assert the remaining leg is untouched and the response is
the mismatch outcome. Without the guard this test deletes the wrong leg, which
is precisely the failure it exists to prevent.

**The composer names the right thread**, in each of the three owner cases,
asserted from the rendered page rather than from the palette or a comment —
the failure mode this codebase keeps hitting is a source-scan test that
matches its own explanatory prose.

## Deferred

- **Live re-search and re-pricing of a leg** (the "D" of this conversation).
  Its own spec, because it decides whether the web client may spend money.
- **Reordering legs.** `drop_segment` renumbers, so the machinery is halfway
  there, but nobody has asked to drag a leg.
- **Showing which chats touched a trip.** The many-to-many is real, but a list
  of threads on a trip card is a feature nobody requested, and single
  ownership is what the composer needs.
