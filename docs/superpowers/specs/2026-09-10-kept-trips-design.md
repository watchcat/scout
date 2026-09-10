# A Trip Is Kept, Not Just Made — Design

## Purpose

A traveller asked Scout for flights from Hong Kong to Fukuoka or Okinawa and
then looked at the Trips tab. It was empty.

Nothing was broken. `add_trip_segment` had been called **zero times** in the
pod's eleven hours, across nine flight-specialist runs and ten searches. The
tool is registered (`flights.rs:148`) and the guidance already says to use it —
*"add_trip_segment for each leg the moment you have a route and a date"* — but
the model does not reach for it on a search, because a search does not feel
like a plan. It has no trip name to use, and inventing one is a commitment
nothing in the request supports.

The obvious fix — always build the trip — has an obvious objection: every
casual price check would litter the Trips tab.

**This removes the objection rather than working around it.** The specialist
builds the trip as it searches, always. The trip is a *draft*, and the Trips
tab does not show drafts. When the traveller says to keep it, one bit changes.

## Not building

- **A "keep" button in the web client.** The chat-native offer works in both
  Telegram and the browser; a button is web-only, and the mirror means the
  same conversation happens in both places. Worth adding later, once the
  chat-native path has proved the model offers at sensible moments.
- **Automatic promotion on any signal** — booking, finalising, choosing an
  option. Each is arguable and none is what was asked for. Keeping is a thing
  the traveller says.
- **Live re-pricing of a kept trip.** Already deferred to its own spec; a
  saved price is labelled with when it was saved, which the trip view already
  does.

## The column

```sql
ALTER TABLE trips ADD COLUMN kept BOOLEAN NOT NULL DEFAULT false;
UPDATE trips SET kept = true;
```

Both statements, in that order, as schema **step 10**. The `UPDATE` is not
housekeeping: every trip that exists today was created under rules where
creating one *was* the act of intent, so every one of them is kept. Without
it, the migration would hide every trip a traveller already has — the exact
complaint this spec exists to answer, inflicted on all their existing data.

`kept`, not `draft`, so the common read is `WHERE kept` rather than a
negation.

## Who sees a draft

This is the whole mechanism, and it is a boundary that already exists in the
code: `Trip` is the model-facing type, `Plan` is the channel-facing one.

- **The model sees everything.** `trip_names` and `show_trip` must include
  drafts, or the specialist cannot find the trip it just built and would
  create a second one on the next message.
- **A channel sees only kept trips.** `trips::list` — what `GET /chat/trips`
  serves — filters to `kept`.

So `Store::list_trips` gains a sibling rather than a flag: the model's read
and the traveller's read are different questions and should not share a
parameter that a caller can get backwards.

## Keeping

A new tool on the flight specialist:

```
keep_trip(trip: String) -> TripView
```

It sets `kept = true` for that account's trip of that name and returns the
trip, like every other trip tool. It performs no search and costs nothing.

It lives on the specialist rather than the main agent because that is where
every other trip tool lives, and because the main agent has no trip tools at
all — it reaches flights only through `ask_flights`. Putting one trip tool
somewhere else would split a coherent set for one case.

The round trip is therefore: traveller says "yes, keep it" → the main agent
briefs `ask_flights` with "keep the trip called X" → the specialist calls
`keep_trip`. No searching, no re-describing an itinerary in prose, and no
opportunity for the flights to be transcribed wrongly on the way — the trip
already exists with the real numbers in it.

Keeping a trip that is already kept is a no-op that returns the trip. Keeping
one that no longer exists returns the same "no trip called X" the other tools
return, which the model relays.

## The offer

`guidance()` in `flights.rs` already computes, in Rust, whether any trip tool
was used (`planned`, line 225). Extend that: when a finding shows a trip was
built **and that trip is still a draft**, guidance carries a line telling the
parent to name the trip and offer to keep it.

Computing it in Rust matters. The alternative — a sentence in the preamble
hoping the model remembers — is exactly what produced the current bug, where
guidance that already said "build the trip" was not enough to make it happen.
A guidance line is data the parent receives with its findings, not a rule it
must recall.

The preamble changes too, and can now be unconditional where it was hedged:
build the trip as you search, every time, because a draft costs nothing and
is invisible until kept.

## What a timer does to a draft

Threads expire after 48 idle hours. The existing rule, from the trip↔chat
link, is that expiry **detaches** a trip rather than deleting it, because a
timer must never destroy a plan.

A draft is not a plan. Nobody kept it.

- Expiry **deletes** unkept drafts owned by the expiring thread.
- Expiry **detaches** kept trips, exactly as now.

This is the line that stops drafts accumulating forever, and it is the right
line rather than a convenient one: the thing a timer should clean up is
precisely the thing nobody asked to keep. A kept trip retains the protection
it has today.

Deleting a thread explicitly is unchanged: it takes both drafts and kept
trips with it, because that was a decision.

## Testing

- **A search builds a draft the Trips tab does not show.** The regression
  test for the reported bug, from both ends: the trip exists in the store,
  and `trips::list` does not return it.
- **The model can see its own draft.** `trip_names` and `show_trip` include
  it. Without this the specialist builds a second trip on the next message,
  which is worse than the bug being fixed.
- **`keep_trip` promotes, and then the tab shows it.** Also that keeping an
  already-kept trip is a no-op rather than an error.
- **Expiry deletes a draft and detaches a kept trip, in the same sweep.**
  One test, both outcomes, so nobody can satisfy it by treating all trips
  alike. This is the test that guards the distinction the whole section
  above exists to draw.
- **The migration backfills.** A hand-built schema-9 database with a trip in
  it must come out with that trip `kept = true`. A fresh-database test proves
  nothing here — the risk is specifically to data that already exists.
- **Guidance offers only when there is a draft to offer.** Not when the trip
  is already kept, not when no trip was built.

## Deferred

- **A web "keep" button**, as above.
- **Naming.** The model picks the trip name. A convention ("route, month")
  belongs in the preamble, but a rule that produces collisions across two
  searches for the same route in the same month needs a real answer, and
  `UNIQUE (account_id, name_key)` makes a collision a silent merge rather
  than an error. Worth its own thought; today's behaviour is unchanged.
- **Sweeping drafts whose thread is still alive.** A traveller who searches
  fifty routes in one long-lived thread accumulates fifty drafts, invisible
  but present. Nothing today bounds that. It is a real question and not this
  one.
