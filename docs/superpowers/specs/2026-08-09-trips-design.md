# Scout — Trips: Design

Date: 2026-08-09
Status: Approved

## Purpose

`search_flights` answers one question at a time: a route, a date, maybe a
return. It cannot express Amsterdam → Lisbon → Rome → Amsterdam, and nothing
it returns survives the conversation that asked for it.

A trip is the missing noun. It lets someone assemble an itinerary over as
many messages as it takes, come back to it tomorrow, and finish with prices
and links that are true at the moment they are handed over.

## Scope

**In scope:**
- Named trips per user, several alive at once
- Ordered segments, each an intent that a chosen flight can be bound to
- Building, amending and inspecting a trip from chat
- Finalising: re-pricing every segment and pricing the whole itinerary as a
  single ticket, then presenting both
- The budget rules that keep finalisation from eating a request's allowance

**Out of scope (deliberately):**
- Watching prices and alerting on changes — a trip is re-priced when asked
- Booking on the traveller's behalf; passengers, seats, payment
- Stays, cars, or anything Duffel's `stays` half offers
- Surface travel between segments as a first-class thing — a gap between one
  segment's arrival airport and the next one's departure is *noticed and
  reported*, not modelled
- Converting currencies to force a total out of a mixed-currency trip

## The constraint that shapes everything

An offer is perishable and a conversation is not.

`Flight.expires_at` exists because Duffel offers die within minutes. Ignav
fares arrive as `Approximate` or `Unconfirmed`, and its `booking_links`
lookup returns `price_now` precisely because "the lookup refreshes the
itinerary, so this can differ from what the search returned". Even
`ShownFlights`, the most generous memory in the codebase, keeps things for
45 minutes and its own doc calls that "long enough to cover a conversation".

So a trip assembled over several turns cannot hold prices. By the time the
third segment is chosen, the first one's quote is a fiction. What survives
is the itinerary — airports, dates, flight numbers, airline. Those are facts
about the world rather than quotes about it.

Everything below follows from that: a trip stores a plan, prices seen while
building are labelled as of a moment, and finalisation is a re-pricing pass
rather than a checkout.

**No offer id is stored.** It is the one field that looks useful and is
guaranteed to be wrong later, and storing it invites a future caller to use
it. Leaving it out makes re-pricing the only path there is.

## Data model

```sql
CREATE SEQUENCE IF NOT EXISTS trips_id_seq;
CREATE TABLE IF NOT EXISTS trips (
    id          BIGINT PRIMARY KEY DEFAULT nextval('trips_id_seq'),
    user_id     BIGINT NOT NULL,
    name        TEXT NOT NULL,
    -- lowercased `name`; the trip is addressed by what the traveller calls
    -- it, and "September" and "september" are the same trip.
    name_key    TEXT NOT NULL,
    adults      BIGINT NOT NULL DEFAULT 1,
    cabin_class TEXT,
    status      TEXT NOT NULL DEFAULT 'planning',
    created_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    UNIQUE (user_id, name_key)
);
CREATE TABLE IF NOT EXISTS trip_segments (
    trip_id            BIGINT NOT NULL,
    position           BIGINT NOT NULL,
    -- The intent. Always present; this is what gets re-searched.
    origin             TEXT NOT NULL,
    destination        TEXT NOT NULL,
    departure_date     TEXT NOT NULL,
    -- The choice. Null until a flight is bound to this segment.
    airline            TEXT,
    flight_numbers     TEXT,
    itinerary          TEXT,
    departing_at_local TEXT,
    arriving_at_local  TEXT,
    duration_minutes   BIGINT,
    -- What it cost when chosen, so finalisation can say the price moved.
    quoted_price       DOUBLE,
    quoted_currency    TEXT,
    quoted_at          TIMESTAMP,
    source             TEXT,
    PRIMARY KEY (trip_id, position)
);
```

`UNIQUE (user_id, name_key)` is enforced by DuckDB — verified against 1.4.5
before relying on it — so a duplicate name is a constraint violation rather
than a check somebody can forget to write.

`flight_numbers` is comma-separated (`KL1007,KL0805`) because finalisation
compares it against fresh results. `itinerary` is the rendered string `Leg`
already produces, for showing. Two fields because matching and displaying
want different shapes, and deriving either from the other loses something.

`position` is 1-based and contiguous. Dropping a segment renumbers the rest:
if positions could have holes, "segment 3" would drift away from the third
thing the traveller can see, and every later instruction would target the
wrong row.

Trips belong to a `user_id`, like purchases and reminders. `ShownFlights` is
keyed by `chat_id`. The consequence is stated under Consequences below.

`adults` and `cabin_class` describe the whole trip rather than a segment —
nobody flies business on one leg of a plan and economy on the next by
accident — and are set through `add_trip_segment`'s optional arguments
whenever they are supplied, last write winning. Every mutating tool echoes
the trip back, so a change to either is visible in the reply rather than
discovered at finalisation.

`status` is `planning` until `finalise_trip` succeeds, and returns to
`planning` the moment any segment changes: the prices it was finalised at
stopped describing the trip when the trip stopped being that trip. It is a
label on what has been priced, not a lock — a finalised trip stays fully
editable.

`quoted_price` and `quoted_at` are never overwritten by finalisation. They
mean "what this cost when you chose it", and refreshing them would make the
price-moved figure shrink to nothing every time it was checked, which is
precisely the number a traveller is looking at it for.

## Tools

Six, which is a real cost against a tool list that is already long. Each is
small, and the alternative — one fat `edit_trip` with an action parameter —
trades that for a model choosing wrongly between modes.

- **`add_trip_segment`** `{ trip, origin, destination, departure_date,
  position?, adults?, cabin_class? }`
  Creates the trip when `trip` matches nothing. IATA codes and dates are
  validated by the existing helpers before anything is written. `position`
  inserts rather than appends, shifting the rest. `adults` and `cabin_class`
  apply to the whole trip whenever supplied.
- **`choose_trip_flight`** `{ trip, position, offer_id }`
  Binds a flight to a segment. `offer_id` must be one `ShownFlights` holds
  for *this chat* — the same guard `BookingLinksTool` uses, for the same
  reason: a model carrying a 32-character id across a turn is the
  invented-ASIN problem in different clothing.
- **`show_trip`** `{ trip? }`
  One trip in full, or with no name, the list of them with status and
  segment count.
- **`drop_trip_segment`** `{ trip, position }`
- **`delete_trip`** `{ trip }`
  Exists because creating a trip is a side effect of a typo, and a typo
  needs an undo.
- **`finalise_trip`** `{ trip }`

Only `add_trip_segment` creates. The rest refuse an unknown name and list
what the user actually has. The asymmetry is deliberate: creating on an
unrecognised name is convenient when adding and destructive when choosing
or deleting.

### Making a wrong target visible

Several named trips means a model can address the wrong one. The mitigation
is structural rather than instructional: **every mutating tool returns the
trip's name and its full current state.** Editing "Setpember" instead of
"September" comes back as a one-segment trip with an unfamiliar name, in
the reply, where the traveller sees it. A preamble rule saying "be careful
which trip" would not survive contact with a long conversation; tool output
the model has to relay does.

## Building a trip

Segments are added in travel order and may be added out of order and fixed
later, so ordering is not enforced while planning — only at finalisation.

Two things are noticed and reported rather than refused:

- **A gap between segments.** Landing at FCO and departing from FLR is a
  legitimate trip with a train in the middle. `show_trip` says so ("you
  arrive at FCO and leave from FLR — that gap is yours to cover") instead of
  pretending the itinerary is continuous or rejecting it.
- **A same-day turnaround** that leaves less than three hours between one
  segment's arrival and the next one's departure, which is the shape of an
  itinerary somebody will miss.

## Finalisation

`finalise_trip` refuses before it spends anything if:

- any segment has no chosen flight — naming which ones, by position
- the departure dates do not run forwards — a multi-slice request built from
  out-of-order dates is meaningless, and silently sorting them would change
  the trip the traveller asked for

Then, concurrently:

1. **Per segment, a fresh search** of its route and date. The chosen
   `flight_numbers` are looked for in the results: finding them gives
   today's price for what they picked, and the cheapest offer now is kept
   alongside so a choice that has aged badly is visible.
2. **One multi-slice Duffel offer request** for the whole itinerary, which
   is what a single ticket would cost.

Ignav has one-way and round-trip endpoints only, so step 2 is Duffel-only.

### Output

Per segment: what was chosen, the price when chosen against the price now
with the difference, and where to buy it. Then both totals:

- **as N separate bookings** — the sum of the segments
- **as one ticket** — the multi-slice offer

and the difference expressed in what it buys, not only in money. `Flight`
already carries `self_transfer` with the note that the traveller "collects
their bags, checks in again, and carries the risk themselves if the first
leg is late". A trip booked as N links is that at *every* join, by
construction, and the comparison says so.

These live in the tool output's `notes`, not in the preamble. The booking
fee taught that lesson already: a disclosure the model is told to make is a
disclosure it sometimes doesn't.

### Totals and currency

A total is produced only when every segment shares a currency. A mixed
trip lists per-segment prices and says there is no total because the
segments are priced in different currencies — inventing a rate to force one
number would be the same class of error as the model doing arithmetic on
local times.

## Budget

Finalising an N-segment trip is N+1 paid searches against a base allowance
of four. `FlightBudget` gains a third component:

```rust
pub fn grant_trip(&self, segments: usize) {
    self.trip.fetch_max(segments + 1, Ordering::Relaxed);
}

pub fn allowance(&self) -> usize {
    BASE_FLIGHT_SEARCHES
        + self.window.load(Ordering::Relaxed)
        + self.trip.load(Ordering::Relaxed)
}
```

`fetch_max` for the same reason `grant_window` uses it: finalising twice in
one request must not buy headroom twice. The two grants are summed rather
than maxed because a flexible-date search and a finalisation are different
work, and a request that genuinely does both needs both.

## Error handling

- **Unknown trip name**, anywhere but `add_trip_segment` → refuse and list
  the user's trip names. Creating on a typo is how a traveller ends up with
  two half-trips and no idea which is which.
- **An offer id this chat was not shown** → refuse and list the valid ids,
  the existing `BookingLinksTool` behaviour.
- **A chosen flight that is no longer offered** → say so and name the
  cheapest on that route now. Do not substitute: the traveller chose a
  flight, not a price band, and quietly swapping it is how a 06:00 departure
  appears in an itinerary nobody agreed to.
- **The multi-slice request returns nothing** → say no airline will build
  the itinerary as one ticket.
- **Duffel not configured** → say the single-ticket comparison could not be
  made. An absent comparison must never read as a verdict for separate
  bookings; that is the failure mode where the design costs somebody money.
- **DuckDB unavailable** → fail closed and change nothing. A half-written
  trip is worse than a refused edit.

## Testing

Store, against a temp database:

- A trip is found by name case-insensitively; two users may both have a
  "September"; one user may not
- Segments come back in `position` order
- Dropping a segment leaves positions contiguous
- Inserting at a position shifts the rest rather than colliding
- Segments and trips are scoped per user
- Finalising sets `status`, and the next edit puts it back to `planning`
- Finalisation leaves `quoted_price` and `quoted_at` alone

Pure functions:

- Matching chosen `flight_numbers` against fresh results, including the
  case where the same numbers appear on a different date
- Totals: summed when currencies agree, refused when they do not
- The price-moved arithmetic and its sign
- Date-order validation
- Gap detection between consecutive segments, and the short-turnaround
  warning

Tools:

- `choose_trip_flight` refuses an id shown in another chat
- `finalise_trip` refuses an incomplete trip and names the segments
- `finalise_trip` output states why a single-ticket comparison is absent
- Every mutating tool returns the trip name and its full state

Budget:

- `grant_trip` raises the allowance by segments + 1
- Two grants in one request do not stack
- A window grant and a trip grant do

## Consequences worth stating

**Half the links do not exist yet.** Ignav segments get real seller links
through `booking_links`. Duffel segments get nothing: `links=false`, and the
preamble already tells the model "Scout cannot book: quote the numbers and
let the user buy from the airline". So a finalised trip hands out links for
some segments and flight numbers for others until Duffel Links is enabled —
still blocked on the `403 unavailable_feature` and the unsent mail to
Duffel. The design works with the asymmetry and is better without it.

**Duffel becomes load-bearing.** Search degrades gracefully to one provider;
the single-ticket half of finalisation does not exist without Duffel. A
Duffel-less deployment gets a trip feature that can only ever recommend
separate bookings, and has to say so every time.

**Trips are per user, shown flights are per chat.** A trip started in a group
chat can be read and edited from a DM, but a flight shown in the group
cannot be bound to it from the DM, because `ShownFlights` is keyed by chat.
That is the right default — one household member should not be able to bind
another's search — but it will read as a bug the first time it happens, so
the refusal message says which chat the flight was shown in.

**The tool list grows by six.** That is the largest single addition to the
agent's surface so far. It is the cost of the trip being a noun the model
can manipulate rather than a shape it has to hold in its head across twenty
turns, but it is worth re-measuring tool-selection quality afterwards.
