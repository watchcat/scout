# One Timeline for a Trip — Design

## Purpose

A trip today is a list of flight segments, each with candidate flights. A
real trip is flights, stays, activities and the train between them, and the
next feature (`2026-09-15-trip-inbox-design.md`) delivers all of those from
forwarded booking confirmations. This replaces segments with items: one
table, one date-ordered list, every kind of thing a traveller books. Flights
keep everything they have today, including candidates and finalising.

This spec is built first and on its own; the inbox lands on it.

## Decisions taken

Settled in conversation; the rest follows from them.

- **Items and flights unified** in one table, flights migrated in. Not a
  second table beside segments and not a bookings list under the trip.
- **Positions, recomputed from the start time on every change.** Not stable
  ids shown as numbers. The known cost: "update item 3" after an insert can
  name a different item. Every tool already returns the whole trip after a
  change and the prompt says to trust the last snapshot, so the renumbering
  is visible each time.
- **Candidates stay a flight-only concept.** A stay or an activity is one
  booking; a hotel search agent, when it exists, can give stays candidates
  the same way.

## Data

Schema steps 14 and 15 (after the debug-trace steps 12 and 13).

```sql
-- step 14
CREATE SEQUENCE IF NOT EXISTS trip_items_id_seq;
CREATE TABLE IF NOT EXISTS trip_items (
    id                BIGINT PRIMARY KEY DEFAULT nextval('trip_items_id_seq'),
    trip_id           BIGINT NOT NULL,
    position          BIGINT NOT NULL,          -- 1-based, recomputed on every write
    kind              TEXT NOT NULL,            -- flight | stay | activity | transport
    title             TEXT NOT NULL,            -- "AMS → LIS", "Hotel Alfama", "Azulejo museum"
    place             TEXT,                     -- city or address; NULL for a flight
    origin            TEXT,                     -- flights and transport
    destination       TEXT,
    starts_at         TEXT,                     -- local ISO datetime or date; see ordering
    ends_at           TEXT,
    date              TEXT NOT NULL,            -- the day it starts, YYYY-MM-DD; the sort key
    booked            BOOLEAN NOT NULL DEFAULT false,
    confirmation_code TEXT,
    price             DOUBLE,
    currency          TEXT,
    notes             TEXT,
    arrival_id        BIGINT,                   -- the email it came from, when it did
    next_candidate    BIGINT NOT NULL DEFAULT 1,
    created_at        TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at        TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE TABLE IF NOT EXISTS item_candidates (
    item_id            BIGINT NOT NULL,
    candidate          BIGINT NOT NULL,
    chosen             BOOLEAN NOT NULL DEFAULT false,
    airline            TEXT NOT NULL,
    flight_numbers     TEXT NOT NULL,
    itinerary          TEXT NOT NULL,
    departing_at_local TEXT,
    arriving_at_local  TEXT,
    duration_minutes   BIGINT,
    quoted_price       DOUBLE,
    quoted_currency    TEXT,
    quoted_at          TIMESTAMP,
    source             TEXT,
    PRIMARY KEY (item_id, candidate)
);
-- Copy every segment as a flight item, then its candidates.
INSERT INTO trip_items (trip_id, position, kind, title, origin, destination, date, next_candidate)
SELECT trip_id, position, 'flight', origin || ' → ' || destination, origin, destination,
       departure_date, next_candidate
FROM trip_segments;
INSERT INTO item_candidates
SELECT i.id, c.candidate, c.chosen, c.airline, c.flight_numbers, c.itinerary,
       c.departing_at_local, c.arriving_at_local, c.duration_minutes,
       c.quoted_price, c.quoted_currency, c.quoted_at, c.source
FROM segment_candidates c
JOIN trip_items i ON i.trip_id = c.trip_id AND i.position = c.position AND i.kind = 'flight';
```

Step 15 is a code step: `reorder_items` for every trip, so positions follow
the date rule below rather than the segment order they were copied with.
The old tables are dropped in a later release, once no code reads them.

`arrival_id` is a plain column now; the inbox spec gives it a table to
point at.

## Ordering

`position` is 1-based and recomputed by `Store::reorder_items(trip_id)`,
called inside every write that adds, changes or drops an item, in the same
transaction. Sort key: `date`, then `starts_at` (nulls after times on the
same day), then `kind` order flight, transport, stay, activity, then the
previous position, so two undated-time items keep their relative order.

A flight item's `date` is its departure date; `starts_at` is set from the
chosen candidate's `departing_at_local` when one is chosen, else null. A
stay's `date` is check-in and `ends_at` check-out. An activity with a date
and no time has `starts_at` null.

## The tools

The flight desk's tools keep their names and their "segment" wording in the
prompt; they read and write items.

- `add_trip_segment(trip, origin, destination, date)` creates a flight item.
- `update_trip_segment`, `drop_trip_segment`, `add_trip_option`,
  `choose_trip_option` address `position` as today and act on the item at
  that position; `add_trip_option` and `choose_trip_option` refuse on a
  non-flight item with a sentence saying why.
- `show_trip`, `delete_trip`, `keep_trip` unchanged in meaning.
- `finalise_trip` prices flight items as today. Non-flight items with a
  price are summed as "fixed costs" in the result; without one they are
  listed as unpriced. `not_ready` looks only at flight items.
- New `add_trip_item(trip, kind, title, place, date, starts_at?, ends_at?,
  notes?)` for a stay, activity or transport from chat: "I booked the
  Alfama hotel for the 12th to the 15th". `booked` is false; a
  confirmation code is not something the model invents.

The `Trip` and `Plan` views list items in position order with `kind`,
`title`, `date`, times, `booked`, `confirmation_code`, and for flights the
candidates as today. The prompt's guidance gains one line: a trip may hold
stays and activities; they are shown, not searched.

## The page

The Trips tab lists items in one column in position order. A flight card is
what it is today. A stay card shows name, place, check-in to check-out, the
booked mark and code. An activity or transport card shows title, place,
date and time. Remove works on any item with the existing confirm. No
editing of non-flight items on the page in this spec; chat does that.

## Testing

- Store: the migration from a version-13 fixture with a two-leg trip, one
  chosen and one undecided candidate, lands as two flight items with their
  candidates re-keyed and positions by date; `reorder_items` on every
  write, including the tie rules; `add_item` for each kind.
- Tools: each flight tool against items; `add_trip_option` refused on a
  stay; `finalise_trip` with a priced stay reports fixed costs and prices
  flights only; `not_ready` ignores non-flights.
- Views: `Trip` and `Plan` carry every kind in position order.
- Page: the client's pure functions for card content per kind; the Rust
  source tests that assert the page's ids.
- Live: a kept trip from before the deploy shows unchanged after it.

## Out of scope

Editing non-flight items on the page; candidates for stays; re-pricing a
kept trip; the inbox (its own spec).
