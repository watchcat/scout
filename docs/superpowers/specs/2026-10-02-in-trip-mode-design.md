# In-Trip Mode — Design

## Purpose

A trip page is built for planning: it opens at the top and treats every day
alike. On a day of the trip the reader wants today, and wants to rearrange
what is left. This adds two things.

1. **Today.** Opened on a date inside the trip, the page marks today, opens
   on today's cards, and tells Scout what day it is.
2. **Moving an item to another day**, from the item's menu or by dragging,
   with Scout checking afterwards whether the item works on that day.

Built as two slices in that order. The first is client-side apart from one
field on a chat message; the second touches the page, a route, the store
and a run.

## Decisions taken

Settled in conversation; the rest follows from them.

- **Both ways to move**: a "Move to…" entry in the item's ⋯ menu, which is
  the primary path and the only one on touch and keyboard, and dragging a
  chip in the day rows on a desktop.
- **The check never blocks a move.** A plan moves at once. A held item asks
  first, without the model: the card moving does not move the booking. The
  check runs after either.
- **The check reads the trip and the web.** Whether the traveller is in
  that city that day, clashes with timed items, and for a named place its
  opening days and hours, two searches at most.
- **The check is an ordinary run of the main agent** in the trip's thread.
  The desk cannot search the web — its tools are flights and trips — so
  the main agent asks the desk for the trip and does the searching. A
  tool-less endpoint was rejected: no search, no record in the chat, and a
  second place that reasons about a trip.
- **Today is the reader's own date**, from the browser. The phone is where
  the traveller is.

## Slice 1 — Today

### Which day is today

`tripDayRows` already yields the trip's days. Today is the browser's local
date at render. It applies only when it falls from the trip's first date to
its last; otherwise nothing on the page changes.

A counted run of empty days ("8 free days") that contains today is split so
today is a row of its own: the run before it, today, the run after it. A
run of one stays a single empty-day row, as now.

### Marking

- Today's row in the day rows: a "Today" kicker under the date and a filled
  dot. Chips keep their held and to-book styling; today is a position, not
  a state.
- Each of today's cards: a 3px left edge in the accent colour and
  `data-today`.
- The held and to-book counts are unchanged.

### Scrolling

On the first paint of a trip that has today in it, the page scrolls to the
first card of today, without animation. If today has no cards it scrolls to
today's row in the overview instead. Later repaints of the same trip (a
note, a move, an Add) do not scroll. Choosing another trip resets this.
The same in the Mini App.

### Telling Scout what day it is

Website only: the Mini App has no composer.

Today a message sent from the Trips tab carries its text and its thread and
nothing else. `sendBody` gains an optional `today` (`YYYY-MM-DD`) and
`trip` (the name), sent only from the Trips tab and only when today is
inside that trip. The server drops both unless the date parses and the
name is one of the account's trips, and otherwise appends a system note to
the prompt, with the weekday worked out from the date:

    [system note] Sent from the trip "Hong Kong, September". Today is
    Wed 23 Sep 2026, a day of that trip.

The thread shows what was typed, not the note. `[system note]` is already
how Telegram appends the price-request rule, and the title logic already
cuts it; the history the page is served must cut it too. The plan verifies
whether it does today and adds that cut where it is missing.

The line above the composer gains "· today is day 3 of 9", counting from
the trip's first date to its last, both included.

### Tests

- Node: `todayRow(rows, today)` — inside the trip, outside it, inside a
  free run (split into three), on the first and last day.
- Node: `sendBody` carries `today` and `trip` only when both are given.
- Rust: the note reaches the prompt; a malformed date is dropped, not
  passed on; the served history shows the typed text without the note.
- Browser, phone and desktop width: first paint lands on today's first
  card; a repaint after a note does not scroll.

## Slice 2 — Moving an item

### What moves

A stay, an activity or a transport. Not a flight: its date is the
ticket's, and the menu entry is not offered on a leg. A stay moves as a
block — check-in moves and check-out moves with it, keeping the nights. An
item's clock keeps its time of day on the new date, which
`Store::update_item` already does.

### Data

Schema step 20, two nullable columns on `trip_items`:

```sql
ALTER TABLE trip_items ADD COLUMN IF NOT EXISTS warning TEXT;
ALTER TABLE trip_items ADD COLUMN IF NOT EXISTS checking_since TIMESTAMP;
```

Both nullable, and nothing made `NOT NULL` in the step: DuckDB refuses
that on a table with rows, which took production down at step 19. The
migration test runs against a database stripped of these columns with an
item in it. `MIGRATIONS` lists them last, in this order.

- `warning` — Scout's last verdict about this item, when it was a warning.
  Cleared by a move, by an edit of the item's date, time or place, and by
  Dismiss.
- `checking_since` — set when a check starts, cleared when it ends for any
  reason.

On the wire a `TripItem` gains `warning: Option<String>` and
`checking: bool`, true while `checking_since` is under five minutes old. A
check that died with the pod stops reading as running after that.

### Routes

Same admission, form token and stale-tab rule as `/chat/trips/item-note`:
the body names the item by position with its title and current date, and a
mismatch is 409 with a reload.

`POST /chat/trips/item-move` — `{trip, position, title, date, to, confirm}`.

| Item | `confirm` | Result |
|---|---|---|
| plan | any | moved, check started, 200 with the trip |
| held | false or absent | nothing written, 200 `{"needs_confirm": true}` |
| held | true | moved, check started, 200 with the trip |
| flight | any | 422 |
| `to` equals `date` | any | nothing written, 200 with the trip |
| `to` not a date | any | 422 |

Checked in this order: the stale-tab rule, a flight, `to` not a date, `to`
equal to `date`, then held without `confirm`.

The held confirmation is the server's rule and not only the page's, so a
client that skips the question does not skip the rule.

`POST /chat/trips/item-warning` — `{trip, position, title, date}` clears
`warning`, 200 with the trip.

### The check

Started by the move route as a background task; the route does not wait.

**Admission.** A check is a request: it is logged as `move_check` and
counts toward the daily cap. Over the cap, or with the account's
model-call limiter refusing, the item moves, no check starts, and the
response says `"checked": false` so the toast can say so.

**The run.** `run_agent` in the trip's thread when that thread is a direct
one, else in a new thread, exactly the rule `composerTarget` applies. The
prompt reads as something the traveller did, then the instructions:

    Moved Lunch with Stanley from Thu 24 Sep to Wed 23 Sep. Check it.

    [system note] The traveller moved this item on the trip
    "Hong Kong, September" themselves; it is already moved. Ask the desk
    what the trip holds. Say whether the item works on its new day: are
    they in that city that day, by the flights and the stay; does it clash
    with anything timed; and if it names a place, search for that place's
    opening days and hours on that date - two searches at most. Do not
    change the trip. End with one line, exactly "verdict: fine" or
    "verdict: warn - <reason in one sentence>".

"Do not change the trip" is an instruction. Nothing in the code enforces
it; the desk can still be asked to edit. Accepted for this version.

**Busy.** A thread runs one run at a time. On `Busy` the task waits ten
seconds and tries again, for two minutes, then gives up: logged, nothing
stored. `Overloaded` is treated the same.

**The verdict.** Parsed from the last non-empty line of the reply,
case-insensitive, with `-` or `—`. `warn` stores the reason in `warning`
and queues one line to the phone through the outbox, keyed on the item and
the date so a repeat does not send twice:

    Lunch with Stanley on Wed 23 Sep: <reason>

`fine` stores nothing. A missing or unreadable line is "unknown": nothing
stored, and the reply is in the chat for anyone who looks. In every case
`checking_since` is cleared.

**Staleness.** A verdict is written only if the item still exists and is
still on the date the check was about. One moved again in the meantime has
its own check.

### The page

**Menu.** "Move to…" in the ⋯ menu of every non-flight item. Choosing it
replaces the menu's contents, as Remove does:

- one row per day from the trip's first date to its last, free days
  included, each with its date and what is already on it;
- the item's own day marked and disabled; today marked;
- a last row, "Another date…", with a date field for a day outside the
  trip;
- for a stay the heading is "Move check-in to…".

A held item, once a day is picked, shows: "Held for Thu 24 Sep. Moving the
card won't move the booking." with Cancel and Move anyway, armed against a
double click as Remove is.

**Drag.** Only where `(hover: hover) and (pointer: fine)`. An item's own
chip in the day rows is draggable; a flight's chip and the derived chips
("Lands at HKG", "check out") are not. Day rows light as drop targets. A
drop on the item's own row does nothing. A held item dropped shows the
same confirmation, in the item's menu popover anchored at the target row.
On touch, chips stay links to their card.

**After a move.** The card repaints in its new place; the page scrolls to
it and focuses it. A toast: "Moved to Wed 23 Sep.", or "Moved to
Wed 23 Sep. Not checked: daily limit reached." While any item of the trip
on screen is `checking`, the page refetches the trips every five seconds,
stopping when none is or when the tab is hidden.

**On the card.** "Checking…" beside the state pill while `checking`. A
warning is a yellow line under the card's head with the reason; the item's
chip in the day rows carries a small mark; "Dismiss warning" is in the ⋯
menu. The printed plan prints the warning line under the item.

### The desk

`update_trip_item` already moves a date, and now clears `warning` as any
date edit does. `FLIGHT_PREAMBLE` gains one rule: moving a held item is
done when asked, with a sentence that the booking itself has not moved.

### Tests

- Store: step 20 on a database with items; a move keeps a stay's nights
  and a clock's time; `warning` cleared by a move, a date, time or place
  edit, and Dismiss; a stale verdict is not written.
- Core: the verdict parser — fine, warn with either dash, mixed case,
  trailing blank lines, absent, garbage; the prompt's wording pinned.
- Routes: each row of the table above; a stale tab is 409; the cap path
  answers `checked: false` and starts nothing.
- Check task, with a stand-in model: a warn stores and nudges once; fine
  stores nothing; `Busy` is retried and then abandoned; `checking_since`
  is cleared on every exit.
- Node: the picker's day list (free days expanded, own day disabled,
  today marked); which chips are draggable; the polling stop rule.
- Browser, phone and desktop width: menu move of a plan, the held
  confirmation, drag and drop, the warning line and its dismissal.

## Out of scope

- Undo on the toast; the reader moves it back through the menu.
- Reordering within a day, and changing an item's time from the page.
- Dragging on touch screens.
- Moving a flight.
- Enforcing "do not change the trip" on a check run.
- A daily morning message, or anything else the bot sends unprompted on a
  trip day.
