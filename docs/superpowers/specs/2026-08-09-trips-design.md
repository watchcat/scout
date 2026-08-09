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
- Ordered segments, each an intent that flights can be bound to
- Several candidate flights on one segment, at most one of them chosen, so a
  decision can be deferred without losing the options
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
- Branches or variants *inside* a trip. Two routings — a stopover in Hong
  Kong against a nonstop — are two different segment lists, so they are two
  named trips, which is what naming them bought. Modelling them as branches
  would duplicate every shared segment or introduce sharing between them,
  and would make the model address a trip *and* a branch right after it was
  given several trips to confuse in the first place.

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
-- The intent: where and when, which is all that gets re-searched.
CREATE TABLE IF NOT EXISTS trip_segments (
    trip_id        BIGINT NOT NULL,
    position       BIGINT NOT NULL,
    origin         TEXT NOT NULL,
    destination    TEXT NOT NULL,
    departure_date TEXT NOT NULL,
    PRIMARY KEY (trip_id, position)
);
-- The options on that segment. Several may sit here undecided; at most one
-- carries `chosen`.
CREATE TABLE IF NOT EXISTS segment_candidates (
    trip_id            BIGINT NOT NULL,
    position           BIGINT NOT NULL,
    candidate          BIGINT NOT NULL,
    chosen             BOOLEAN NOT NULL DEFAULT false,
    airline            TEXT NOT NULL,
    flight_numbers     TEXT NOT NULL,
    itinerary          TEXT NOT NULL,
    departing_at_local TEXT,
    arriving_at_local  TEXT,
    duration_minutes   BIGINT,
    -- What it cost when parked, so finalisation can say the price moved.
    quoted_price       DOUBLE,
    quoted_currency    TEXT,
    quoted_at          TIMESTAMP,
    source             TEXT,
    PRIMARY KEY (trip_id, position, candidate)
);
```

Splitting the two tables is what candidates force, and it is better than
what it replaces: the intent columns are required and the option columns are
required *within an option*. The single-table version had every choice column
nullable, which is a way of writing "this row is in one of two states" and
hoping the reader notices.

"At most one chosen per position" is not expressible as a `UNIQUE` — `false`
repeats. It is enforced in Rust instead, by clearing the position's flags and
setting one inside a single `Store` method: the connection is behind one
mutex, so that method is atomic by construction, the same property the
finalisation refusals rely on.

**Candidate numbers are handed out from a per-segment counter and never
taken back.** The obvious implementation — one more than the highest number
currently on the segment — recycles a number the moment the highest option
is dropped, and the number is precisely what a later "go with option 2"
refers to. Somebody shown two options, who drops the second and adds
another, would be given a different flight under a name they already had an
opinion about, with nothing signalling the substitution. So the counter
lives on the segment row, rises only, and travels with the segment through
position shifts.

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
`planning` on any edit that changes what would be priced: a segment, its
options, the passenger count, or the cabin. The prices it was finalised at
stopped describing the trip when the trip stopped being that trip, and a
trip priced for one adult is not priced for two. It is a label on what has
been priced, not a lock — a finalised trip stays fully editable.

An upsert that supplies no new values changes nothing and must not reset
anything: that call is how find-or-create works, and making it destructive
would mean merely naming a trip un-priced it.

`quoted_price` and `quoted_at` are never overwritten by finalisation. They
mean "what this cost when you chose it", and refreshing them would make the
price-moved figure shrink to nothing every time it was checked, which is
precisely the number a traveller is looking at it for.

## Tools

Seven, which is a real cost against a tool list that is already long. Each is
small, and the alternative — one fat `edit_trip` with an action parameter —
trades that for a model choosing wrongly between modes.

- **`add_trip_segment`** `{ trip, origin, destination, departure_date,
  position?, adults?, cabin_class? }`
  Creates the trip when `trip` matches nothing. IATA codes and dates are
  validated by the existing helpers before anything is written. `position`
  inserts rather than appends, shifting the rest. `adults` and `cabin_class`
  apply to the whole trip whenever supplied.
- **`add_trip_option`** `{ trip, position, offer_id, decided? }`
  Parks a flight against a segment as a candidate. `offer_id` must be one
  `ShownFlights` holds for *this chat* — the same guard `BookingLinksTool`
  uses, for the same reason: a model carrying a 32-character id across a
  turn is the invented-ASIN problem in different clothing. `decided`
  defaults to true and also marks the candidate chosen, so the ordinary
  "this is the flight" path is still one call; `decided: false` is "keep
  this one in the running".
  A flight must match the segment it is bound to on **origin, destination
  and date**. Route alone is not enough: airlines reuse flight numbers day
  to day, so a flight parked against the wrong date does not merely fail to
  re-price — at finalisation it can match a different aircraft carrying the
  same number, and price that instead. The check happens inside the same
  critical section as the write, because a position shift between validating
  and writing would otherwise land a validated candidate on a segment that
  is no longer the one that was checked.
- **`choose_trip_option`** `{ trip, position, candidate }`
  Settles a segment later, by candidate number. It exists because deciding
  happens after the offer that produced the option has expired, so there is
  no id left to name it by.
- **`show_trip`** `{ trip? }`
  One trip in full, or with no name, the list of them with status and
  segment count. Undecided segments are shown as such, with their
  candidates numbered — those numbers are what `choose_trip_option` takes.
- **`drop_trip_segment`** `{ trip, position, candidate? }`
  Without `candidate`, the whole segment. With one, just that option, so a
  rejected choice can be cleared without losing the segment.
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

### Undecided segments

"Via Hong Kong or direct, I haven't decided" is two different requests
depending on what "via" means, and only one of them is about candidates.

**A connection through Hong Kong** is an ordinary one-stop flight. It is the
*same segment* as the nonstop — same origin, destination and date — and the
two are simply different offers on it. `search_flights` already has
`max_connections`, and `Leg.connections` already carries the airport and
layover. Both go on one segment as candidates, and the decision is deferred
until `choose_trip_option`.

**A stopover in Hong Kong** — days there on the way — is not one segment at
all. It is AMS→HKG and HKG→NRT against a single AMS→NRT: two different
segment lists, and therefore two named trips, finalised and compared.

Candidates cost nothing at finalisation, which is the property that makes
them worth having. A segment is re-priced by one search of its route and
date, and that one search returns every candidate sitting on it. Pricing
three options for a segment is the same paid request as pricing one, so the
finalisation output can show what each of them costs now — which is exactly
the number the decision was waiting for.

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

- any segment has no candidates at all — naming which ones, by position
- any segment has more than one candidate and none chosen — naming the
  segment and listing its options, since that refusal is a question. A
  segment with exactly one candidate needs no `chosen` flag: it is the pick
  by elimination, and demanding a decision nobody has a choice about is
  ceremony
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

Per segment: what was chosen, the price when parked against the price now
with the difference, and where to buy it. Where a segment carried more than
one candidate, the runners-up are listed with *their* prices now too — they
came free with the same search, and a decision made a week ago deserves to
be checked against what the alternatives cost today. Then both totals:

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

Note `segments`, not candidates. A segment carrying three options is still
one search, so the allowance does not move when an option is parked. That is
a property worth a test rather than a comment: if it ever stopped being
true, deferring a decision would start costing money and nothing would say
so.

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
- Several candidates sit on one position, and at most one is ever `chosen`
- Choosing a second candidate clears the first
- Dropping a candidate leaves the segment; dropping the segment takes them all

Pure functions:

- Matching chosen `flight_numbers` against fresh results, including the
  case where the same numbers appear on a different date
- Totals: summed when currencies agree, refused when they do not
- The price-moved arithmetic and its sign
- Date-order validation
- Gap detection between consecutive segments, and the short-turnaround
  warning

Tools:

- `add_trip_option` refuses an id shown in another chat
- `add_trip_option` with `decided` defaulted marks the candidate chosen, so
  the one-option path needs no second call
- `choose_trip_option` refuses a candidate number the segment does not have
- `finalise_trip` refuses a segment with two candidates and no choice, and
  lists them
- `finalise_trip` accepts a segment with one undecided candidate
- `finalise_trip` refuses an empty segment and names it
- `finalise_trip` output states why a single-ticket comparison is absent
- Every mutating tool returns the trip name and its full state

Budget:

- `grant_trip` raises the allowance by segments + 1
- Parking extra candidates does not raise it
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

**The tool list grows by seven.** That is the largest single addition to the
agent's surface so far. It is the cost of the trip being a noun the model
can manipulate rather than a shape it has to hold in its head across twenty
turns, but it is worth re-measuring tool-selection quality afterwards.
