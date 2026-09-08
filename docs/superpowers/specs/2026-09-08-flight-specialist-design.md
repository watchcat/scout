# A Flight Agent the Main Agent Calls — Design

## Purpose

One agent answers everything today: it carries about twenty-five tools and a
system prompt of which roughly half is about flights, booking links and
trips, whether the question is a flight to Lisbon or a bottle of detergent.
This splits flights out into a specialist agent that the main agent calls as
a tool. A shopping run stops paying for the flight rules, the flight rules get
a prompt of their own, and each agent can later run on its own model.

The wrapper is general. Flights are the first specialist; a trip builder, a
hotel agent and an experience agent come later as new instances of the same
thing, and a specialist can hold another specialist as a tool, which is how
the trip builder will call the flight agent.

## Decisions taken

Settled in conversation; the rest of the document follows from them.

- **Goal order: cheaper and sharper product runs first**, better flight
  answers second, a model per agent third.
- **Everything flight-shaped moves**: `search_flights`,
  `flight_booking_links`, `create_booking_link` and all seven trip tools live
  in the flight agent. The main agent knows only that `ask_flights` exists.
- **The specialist sees only a brief.** The main agent writes a
  self-contained request; the flight agent has no history and keeps none.
  Its report carries offer ids, so "book the second one" becomes a new brief
  that names the id.
- **The main agent writes the prose.** The flight agent returns structured
  findings, not chat text, so a trip builder can consume them later.
- **The presentation rules travel with the result.** The flight tool's
  output carries a guidance block generated in Rust from what the specialist
  actually did. The main prompt keeps one short rule about flights.

## Architecture

```
user ──▶ run_agent ──▶ main agent (shopping prompt, shopping tools, ask_flights)
                            │
                            │ ask_flights { brief }
                            ▼
                       Specialist (rig Tool)
                            │ streams a nested run, forwards progress,
                            │ collects every nested tool's output
                            ▼
                       flight agent (flight prompt, search_flights,
                       booking links, trip tools)
                            │
                            ▼
                       Report { summary, findings, guidance } ──▶ main agent
```

The nested transcript is thrown away. The main conversation records the
`ask_flights` call and its report, exactly as it records a `search_flights`
call and its output today.

## Components

### `specialist.rs` (new, scout-core)

`Specialist` implements rig's `Tool`.

- Fields: the tool name the parent sees, its description, the nested
  `Agent`, a clone of the run's `EventSink`, and a deadline.
- Args: `{ "brief": string }`. Description tells the parent what a brief
  must contain; the flight instance says route, dates, passengers, cabin,
  flexibility and any offer id the user is pointing at.
- `call` streams the nested agent with the brief as its only prompt and no
  history. For every `ToolExecutionStart` it emits
  `AgentEvent::Tool(describe(name, args))` on the sink, so nested searches
  show in the chat as progress. For every `ToolResult` it records a
  `Finding { tool, args, output }`, where `output` is the tool's own JSON,
  parsed back from the result text.
- Output: `Report { summary, findings, guidance }`. `summary` is the nested
  agent's final text with thinking stripped. `guidance` comes from a
  closure the instance supplies, `Fn(&[Finding]) -> Vec<String>`, so the
  wrapper knows nothing about flights.
- The nested run has its own deadline, `SPECIALIST_BUDGET = 3 min`, inside
  the outer `RUN_BUDGET`. The outer stall guard is not tripped by a long
  nested run because progress events keep flowing.
- Failure: if the nested run errors, times out or hits its turn cap, the
  report is still returned with the findings collected so far and a summary
  that says what went wrong. Only when no finding was collected does `call`
  return an error, whose text is a plain sentence for the model to relay.
  Two routes paid for before a failure on the third still come back.

### `flights.rs` (new, scout-core)

- `build_flight_agent(deps, run, facts, budget, events)` builds the nested
  rig agent: preamble `FLIGHT_PREAMBLE` filtered through
  `rules_for_available_tools`, model from `deps.flight_model`, turn cap
  `FLIGHT_TURNS = 12`, and the tools below.
- Tools, moved from `build_agent` with their wiring unchanged:
  `search_flights` (Duffel and Ignav, Ignav wrapped with `fare_market`),
  `flight_booking_links` when Ignav is configured, `create_booking_link`
  when Duffel, Links and a return URL exist, `finalise_trip`, and the trip
  tools `add_trip_segment`, `add_trip_option`, `choose_trip_option`,
  `show_trip`, `update_trip_segment`, `drop_trip_segment`, `delete_trip`.
- `ask_flights(deps, run, facts, budget, events) -> Specialist` wraps it
  with the tool name `ask_flights` and `guidance` below.
- `FLIGHT_PREAMBLE` is the working half of today's flight rules: it asks
  the airlines rather than the web, works out airport codes itself, passes
  `flex_days` only when dates are flexible and never more than 3, never
  repeats a price from earlier, uses the trip tools when a plan spans more
  than one leg, writes the plan down as it goes, never deletes and rebuilds
  a trip, trusts each call's `changed` line and the last snapshot, and
  finalises only when the trip is settled. It ends with: finish with one or
  two sentences saying what was searched and any caveat, and no prices,
  because the numbers travel in the findings. The two booking rules keep
  the "When `<tool>` is available," opening so the filter drops them with
  their tool.
- `guidance(findings, markup_rate) -> Vec<String>` returns the presentation
  half, by section, each included only when it applies. `ask_flights`
  captures the rate in the closure it hands the wrapper.
  - `search_flights` ran: the Cheapest / Fastest / Best balance headings and
    the rule to offer every pick; the itinerary line copied exactly; the
    `price_status` wording; `self_transfer` and `changes_airport`; layover
    from `connections`, never from local times; `duration` for journey
    length; `found: 0` means nothing flies; prices are for all passengers;
    one option per block with the link on its own line.
  - any `search_flights` output has a non-empty `by_date`: present the
    cheapest per day as a short list, say which days were covered.
  - `flight_booking_links` ran: airline link first, resellers with both
    prices, the re-checked fare replaces the old one.
  - `create_booking_link` ran: what the Duffel checkout is, that it cannot
    be pre-filled, single-use and short-lived.
  - any trip tool ran: quote parked prices as of when they were parked,
    `not_ready` means the trip cannot be priced, present both totals from
    `finalise_trip` and never drop the separate-tickets note.
  - `markup_rate > 0`: prices already include the fee, say so once.

### `agent.rs`

- `PREAMBLE` loses every flight and trip rule, the booking-fee paragraph,
  and the "flights have no product page" clause. It gains one conditional
  rule: "When ask_flights is available, send it every question about
  flights, fares, airports, booking a flight, or a trip being planned, with
  a self-contained brief: route, dates, passengers, cabin, flexibility, and
  any offer id the user is pointing at. Never search the web for a flight
  and never compare_prices one. Present its findings following the guidance
  in its result, and take every number from the findings verbatim."
- `preamble_with_profile` no longer appends the booking-fee line; the fee
  now arrives in the guidance and in `search_flights` notes.
- `ALL_TOOLS = ["search_bol", "ask_flights"]`; `available_tools` gates
  `ask_flights` on `duffel.is_some() || ignav.is_some()`.
- `build_agent(d, run, facts, events)` gains the sink. It creates the
  `FlightBudget` as today and registers `ask_flights` where it registered
  the flight tools.
- `AgentDeps` gains `flight_model: String`.
- `wrap_up_agent` is unchanged apart from the shorter preamble.

### `config.rs`

`FLIGHT_MODEL`, optional, default `MODEL`. `for_test` leaves the default.

### `describe.rs`

New arms so nested calls read as progress rather than "⚙️ search_flights":
`ask_flights` shows the brief, `search_flights` the route and date and the
window when `flex_days` is set, `flight_booking_links` and
`create_booking_link` as "🔗 booking link", `finalise_trip` as "✈️ pricing
the trip", the other trip tools as "🗺️ updating the trip".

### `run.rs`

Passes `events.clone()` into `build_agent`. Nothing else changes: the
report lands in history through rig as any tool result does.

## Data flow for one flight question

1. User: "cheapest return AMS to LIS, 12 to 19 October, 2 adults, dates can
   move a day".
2. Main agent, turn 1: `ask_flights { brief: "return AMS→LIS, out
   2026-10-12 back 2026-10-19, 2 adults, economy, flexible ±1 day" }`.
3. Specialist streams the flight agent. It calls `search_flights` with
   `flex_days: 1`; the chat shows "✈️ searching AMS→LIS 12 Oct ±1". The
   output is recorded as a finding. The flight agent answers "Searched the
   return with a day either side; the 11th is cheapest outbound."
4. Report: that summary, one finding, and the guidance sections for
   `search_flights`, `by_date` and, when charged, the fee.
5. Main agent, turn 2: writes the reply from the finding under the
   guidance.

A follow-up "book the second one" is turn 1 `ask_flights { brief: "booking
links for offer_id ign_…" }`, the flight agent calls
`flight_booking_links`, and the report carries the links and their
guidance.

## Budgets and limits

- One `FlightBudget` per user request, shared by every `ask_flights` call
  in that request, as `search_flights` and `finalise_trip` share it today.
- `SPECIALIST_BUDGET` 3 minutes inside `RUN_BUDGET`; `FLIGHT_TURNS` 12.
- The main agent's `MAX_TURNS` stays 20. A flight question now costs it two
  or three turns instead of six to ten.
- The `shown` memo is keyed on the conversation and outlives the request,
  so a booking lookup in a later message still verifies its offer id.

## Testing

Test-first throughout, the model endpoint on the closed test port so
nothing reaches a provider.

- `specialist.rs`: a report is assembled from collected findings; guidance
  is whatever the supplied function returns for those findings; an error
  only when nothing was collected; progress events arrive on the sink in
  call order; a nested failure after a finding still returns the finding.
  Driven by a stub tool set and a stub model where rig allows it, and by
  the closed port for the failure path.
- `flights.rs`: each guidance section appears exactly when its trigger is
  present; the `by_date` section keys on the output, not the args; the two
  booking rules drop with their tool; `FLIGHT_PREAMBLE` contains no
  presentation words ("itinerary line", "Cheapest", "price_status") and
  `PREAMBLE` contains none of the moved rules ("search_flights",
  "flex_days", "itinerary", "add_trip_segment", "Booking fee").
- `agent.rs`: `ask_flights` gated exactly as `search_flights` was; the
  existing preamble-versus-tools tests updated to the new name; the
  booking-fee line is gone from the main preamble.
- `describe.rs`: every flight and trip tool name reads as a sentence.
- `run.rs`: source assertion that `build_agent` receives the sink.
- `config.rs`: `FLIGHT_MODEL` defaults to `MODEL` and can be set.

After merge: one live flight question and one "book the second one"
follow-up on the deployed pod, watching the progress lines and the reply.

## Out of scope

The trip builder, hotel and experience agents; a model per agent beyond
the config value; token accounting for the nested run (the board's
"token usage per run" card covers it); streaming the specialist's
reasoning to the chat.
