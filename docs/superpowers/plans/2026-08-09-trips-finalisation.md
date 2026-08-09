# Trips: Finalisation — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `finalise_trip` re-prices every segment of a built trip, asks Duffel what the whole itinerary costs as a single ticket, and presents both totals with the difference stated in risk rather than only in money.

**Architecture:** Finalisation is a re-pricing pass, never a checkout — an offer expires in minutes and a plan outlives the conversation, so nothing stored is bookable and everything quoted is fetched fresh. Per-segment searches reuse the same two-provider merge `search_flights` uses, extracted from `FlightSearchTool::one_day` so a re-price sees exactly what the original search saw.

**Tech Stack:** Rust 2021, rig 0.40 tools, reqwest, tokio, serde.

**Spec:** `docs/superpowers/specs/2026-08-09-trips-design.md`

**Depends on:** `2026-08-09-trips-storage-and-building.md` must be complete. This plan uses `Trip`, `TripSegment`, `TripCandidate`, `Store::find_trip`, `Store::set_trip_status`, `dates_run_forwards` and `StoreToolError` from it.

---

## File Structure

| File | Responsibility |
|---|---|
| `src/tools/budget.rs` (modify) | `FlightBudget::grant_trip`, so finalising an N-segment trip is not refused by an allowance sized for four. |
| `src/tools/duffel.rs` (modify) | `Slice`, `MultiCityQuery` and `DuffelClient::search_multi_city`; extract the provider merge out of `one_day` into a reusable `merged_search`. |
| `src/tools/trips.rs` (modify) | Matching, price movement, totals, and the `finalise_trip` tool. |
| `src/agent.rs` (modify) | Register the tool; preamble. |

---

### Task 1: The allowance follows the trip

**Files:**
- Modify: `src/tools/budget.rs`

- [ ] **Step 1: Write the failing test**

Add to `mod flight_budget_tests` in `src/tools/budget.rs`:

```rust
#[test]
fn finalising_a_trip_buys_the_searches_it_needs() {
    // A four-segment trip is four re-prices plus one multi-city request.
    // Against a base of four it would be refused halfway through, with
    // half a trip priced and no way to say which half.
    let budget = FlightBudget::default();
    budget.grant_trip(4);
    assert_eq!(budget.allowance(), BASE_FLIGHT_SEARCHES + 5);
    for _ in 0..BASE_FLIGHT_SEARCHES + 5 {
        assert!(budget.claim_one());
    }
    assert!(!budget.claim_one());
}

#[test]
fn finalising_twice_does_not_buy_headroom_twice() {
    let budget = FlightBudget::default();
    budget.grant_trip(4);
    budget.grant_trip(4);
    budget.grant_trip(2);
    assert_eq!(budget.allowance(), BASE_FLIGHT_SEARCHES + 5);
}

#[test]
fn a_flexible_search_and_a_finalisation_each_get_their_own_room() {
    // Different work, so they add rather than compete: a request that
    // genuinely does both needs both.
    let budget = FlightBudget::default();
    budget.grant_window(1);
    budget.grant_trip(2);
    assert_eq!(budget.allowance(), BASE_FLIGHT_SEARCHES + 2 + 3);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet finalising_a_trip_buys`
Expected: FAIL to compile — `no method named grant_trip`.

- [ ] **Step 3: Implement**

In `src/tools/budget.rs`, add a field to `FlightBudget`:

```rust
    /// Extra searches granted for finalising a trip: one per segment, plus
    /// the multi-city request that prices the whole thing as one ticket.
    trip: AtomicUsize,
```

and the methods:

```rust
    /// Room for pricing a whole trip. Counts segments, not candidates: a
    /// segment carrying three options is still one search, so deferring a
    /// decision never costs anything.
    pub fn grant_trip(&self, segments: usize) {
        self.trip.fetch_max(segments + 1, Ordering::Relaxed);
    }
```

and change `allowance` to:

```rust
    pub fn allowance(&self) -> usize {
        BASE_FLIGHT_SEARCHES
            + self.window.load(Ordering::Relaxed)
            + self.trip.load(Ordering::Relaxed)
    }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --quiet budget::`
Expected: all pass, including the pre-existing window tests.

- [ ] **Step 5: Commit**

```bash
git add src/tools/budget.rs
git commit -m "feat: let the flight allowance cover finalising a trip"
```

---

### Task 2: Price a whole itinerary as one ticket

**Files:**
- Modify: `src/tools/duffel.rs`

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `src/tools/duffel.rs`:

```rust
#[test]
fn a_multi_city_request_carries_every_slice_in_one_offer_request() {
    // One request, so the airlines can build it as a single fare with
    // protected connections. Four separate requests cannot produce that
    // answer no matter how they are added up.
    let query = MultiCityQuery {
        slices: vec![
            Slice::new("AMS", "LIS", "2026-09-03"),
            Slice::new("LIS", "FCO", "2026-09-07"),
            Slice::new("FCO", "AMS", "2026-09-12"),
        ],
        adults: 2,
        cabin_class: Some("business".to_string()),
    };
    let body = query.body();
    let slices = body["data"]["slices"].as_array().unwrap();
    assert_eq!(slices.len(), 3);
    assert_eq!(slices[1]["origin"], "LIS");
    assert_eq!(slices[1]["destination"], "FCO");
    assert_eq!(slices[2]["departure_date"], "2026-09-12");
    assert_eq!(body["data"]["passengers"].as_array().unwrap().len(), 2);
    assert_eq!(body["data"]["cabin_class"], "business");
}

#[test]
fn a_multi_city_request_needs_at_least_two_slices() {
    // One slice is an ordinary search, and sending it here would bill a
    // second time for an answer already in hand.
    let query = MultiCityQuery {
        slices: vec![Slice::new("AMS", "LIS", "2026-09-03")],
        adults: 1,
        cabin_class: None,
    };
    assert!(query.validate().is_err());

    let bad = MultiCityQuery {
        slices: vec![Slice::new("Amsterdam", "LIS", "2026-09-03"), Slice::new("LIS", "AMS", "2026-09-07")],
        adults: 1,
        cabin_class: None,
    };
    let err = bad.validate().unwrap_err().to_string();
    assert!(err.contains("IATA"), "got: {err}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet a_multi_city_request_carries`
Expected: FAIL to compile — `cannot find struct MultiCityQuery`.

- [ ] **Step 3: Implement the types**

Add to `src/tools/duffel.rs`, near `FlightQuery`:

```rust
/// One direction on one date. The unit a trip segment is priced in.
#[derive(Debug, Clone, PartialEq)]
pub struct Slice {
    pub origin: String,
    pub destination: String,
    pub departure_date: String,
}

impl Slice {
    pub fn new(origin: &str, destination: &str, departure_date: &str) -> Self {
        Self {
            origin: origin.to_string(),
            destination: destination.to_string(),
            departure_date: departure_date.to_string(),
        }
    }
}

/// A whole itinerary priced as a single ticket.
///
/// This is the half of finalisation that separate per-segment searches
/// cannot answer: connections on one ticket are protected, and the fare is
/// often not the sum of its parts in either direction.
#[derive(Debug, Clone)]
pub struct MultiCityQuery {
    pub slices: Vec<Slice>,
    pub adults: u32,
    pub cabin_class: Option<String>,
}

impl MultiCityQuery {
    pub fn validate(&self) -> Result<(), DuffelError> {
        if self.slices.len() < 2 {
            return Err(DuffelError::Invalid(
                "a single-ticket comparison needs at least two slices; one is an ordinary search"
                    .to_string(),
            ));
        }
        for slice in &self.slices {
            let origin = iata_code("origin", &slice.origin)?;
            let destination = iata_code("destination", &slice.destination)?;
            if origin == destination {
                return Err(DuffelError::Invalid(format!(
                    "a slice leaves and arrives at {origin}"
                )));
            }
            calendar_date("departure_date", &slice.departure_date)?;
        }
        Ok(())
    }

    pub fn body(&self) -> serde_json::Value {
        let slices: Vec<serde_json::Value> = self
            .slices
            .iter()
            .map(|s| {
                serde_json::json!({
                    "origin": s.origin.trim().to_ascii_uppercase(),
                    "destination": s.destination.trim().to_ascii_uppercase(),
                    "departure_date": s.departure_date.trim(),
                })
            })
            .collect();
        let passengers: Vec<serde_json::Value> =
            (0..self.adults.max(1)).map(|_| serde_json::json!({"type": "adult"})).collect();
        let mut data = serde_json::json!({"slices": slices, "passengers": passengers});
        if let Some(cabin) = &self.cabin_class {
            data["cabin_class"] = serde_json::json!(cabin.trim().to_ascii_lowercase());
        }
        serde_json::json!({"data": data})
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --quiet a_multi_city_request`
Expected: 2 passed.

- [ ] **Step 5: Extract the request so both searches share it**

Replace the body of `DuffelClient::search` with a call to a new private method, and add the multi-city entry point:

```rust
    pub async fn search(&self, query: &FlightQuery) -> Result<Vec<Flight>, DuffelError> {
        // Checked before the request, not after: Duffel bills per search, so
        // a call the model got wrong should cost nothing.
        query.validate()?;
        self.offer_request(query.body()).await
    }

    /// The whole itinerary as one ticket.
    pub async fn search_multi_city(
        &self,
        query: &MultiCityQuery,
    ) -> Result<Vec<Flight>, DuffelError> {
        query.validate()?;
        self.offer_request(query.body()).await
    }

    /// `POST /air/offer_requests` for any body. Shared so a one-way, a
    /// return and a multi-city trip cannot drift apart in how they are sent
    /// or how their prices get the markup applied.
    async fn offer_request(
        &self,
        body: serde_json::Value,
    ) -> Result<Vec<Flight>, DuffelError> {
        let resp = self
            .http
            .post(format!("{}/air/offer_requests", self.base_url))
            .bearer_auth(&self.api_key)
            .header("Duffel-Version", DUFFEL_VERSION)
            .header("Accept", "application/json")
            // Without this the offers arrive empty and each search needs a
            // second round trip to be worth anything.
            .query(&[("return_offers", "true")])
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(DuffelError::Api {
                status: status.as_u16(),
                body: text.chars().take(300).collect(),
            });
        }
        let mut flights = parse_offers(&text)?;
        // Quoted with the fee already on, because Duffel Links will charge
        // it: saying 600 and billing 618 is exactly the gap this project
        // exists to close. A uniform rate cannot reorder the set, so the
        // ranking means the same thing either way.
        if self.markup_rate > 0.0 {
            for flight in &mut flights {
                flight.price = round_money(flight.price * (1.0 + self.markup_rate));
            }
        }
        Ok(flights)
    }
```

- [ ] **Step 6: Extract the two-provider merge**

`one_day` currently owns the logic that asks both providers and tolerates one of them failing. Finalisation needs exactly that, so lift it out. Add this free function to `src/tools/duffel.rs`:

```rust
/// Both configured providers, asked for the same slice at once.
///
/// One provider failing must not lose the other's answer; only every
/// configured provider failing is a failed search. Shared by `search_flights`
/// and by trip finalisation so a re-price sees what the original search saw.
pub async fn merged_search(
    duffel: Option<&DuffelClient>,
    ignav: Option<&crate::tools::ignav::IgnavClient>,
    day: &FlightQuery,
) -> Result<Vec<Flight>, DuffelError> {
    let (from_duffel, from_ignav) = futures::future::join(
        async {
            match duffel {
                Some(client) => Some(client.search(day).await.map_err(|e| e.to_string())),
                None => None,
            }
        },
        async {
            match ignav {
                Some(client) => Some(client.search(day).await.map_err(|e| e.to_string())),
                None => None,
            }
        },
    )
    .await;

    let mut flights = Vec::new();
    let mut failure = None;
    for (name, result) in [("duffel", from_duffel), ("ignav", from_ignav)] {
        match result {
            Some(Ok(found)) => flights.extend(found),
            Some(Err(e)) => {
                tracing::warn!(error = %e, provider = name, "flight provider failed");
                failure = Some(DuffelError::Provider(format!("{name}: {e}")));
            }
            None => {}
        }
    }
    if flights.is_empty() {
        if let Some(e) = failure {
            return Err(e);
        }
    }
    Ok(flights)
}
```

Then replace the body of `FlightSearchTool::one_day` between the memo check and `self.budget.remember(...)` with:

```rust
        let found = merged_search(self.duffel.as_ref(), self.ignav.as_ref(), &day).await;
        self.note_search().await;
        let flights = match found {
            Ok(flights) => flights,
            Err(e) => return (day, Err(e)),
        };

        self.budget.remember(key, flights.clone());
        (day, Ok(flights))
```

- [ ] **Step 7: Verify nothing regressed**

Run: `cargo test --quiet duffel::`
Expected: every pre-existing duffel test still passes — the extraction changes no behaviour.

- [ ] **Step 8: Commit**

```bash
git add src/tools/duffel.rs
git commit -m "feat: price a whole itinerary as one Duffel ticket"
```

---

### Task 3: Finding a parked option in fresh results

**Files:**
- Modify: `src/tools/trips.rs`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/tools/trips.rs`:

```rust
fn parked(numbers: &str, price: f64) -> TripCandidate {
    TripCandidate {
        candidate: 1,
        chosen: true,
        airline: "KLM".to_string(),
        flight_numbers: numbers.to_string(),
        itinerary: "somewhere".to_string(),
        departing_at_local: None,
        arriving_at_local: None,
        duration_minutes: None,
        quoted_price: Some(price),
        quoted_currency: Some("EUR".to_string()),
        source: Some("duffel".to_string()),
    }
}

#[test]
fn a_parked_option_is_recognised_by_its_flight_numbers() {
    let offers = vec![
        one_way("a", "AMS", "NRT", "2026-09-03", &["KL861"]),
        one_way("b", "AMS", "NRT", "2026-09-03", &["CX270", "CX500"]),
    ];
    let found = match_candidate(&parked("CX270,CX500", 780.0), &offers).unwrap();
    assert_eq!(found.offer_id, "b");

    // Order matters: the same two numbers flown the other way round is a
    // different itinerary.
    assert!(match_candidate(&parked("CX500,CX270", 780.0), &offers).is_none());
    assert!(match_candidate(&parked("KL999", 780.0), &offers).is_none());
}

#[test]
fn a_price_that_moved_is_reported_with_its_sign() {
    // The number the traveller opens this output for.
    assert_eq!(price_move(Some(118.0), Some(131.0)), Some(13.0));
    assert_eq!(price_move(Some(131.0), Some(118.0)), Some(-13.0));
    assert_eq!(price_move(None, Some(131.0)), None);
    assert_eq!(price_move(Some(118.0), None), None);
    // Rounded to the cent: a float subtraction otherwise reports
    // 12.999999999999986.
    assert_eq!(price_move(Some(118.01), Some(131.0)), Some(12.99));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet a_parked_option_is_recognised`
Expected: FAIL to compile — `cannot find function match_candidate`.

- [ ] **Step 3: Implement**

Add to `src/tools/trips.rs`:

```rust
use crate::store::TripCandidate;

/// The offer in `offers` that is the same itinerary as `candidate`, if it is
/// still sold.
///
/// Flight numbers in order are the identity: they are what survives an offer
/// expiring, which is the whole reason a trip stores them instead of an id.
/// Order is part of it — the same two numbers flown the other way round is a
/// different journey.
pub fn match_candidate<'a>(
    candidate: &TripCandidate,
    offers: &'a [Flight],
) -> Option<&'a Flight> {
    let wanted = candidate.flight_numbers.trim();
    offers.iter().find(|offer| {
        let numbers = offer
            .legs
            .iter()
            .flat_map(|leg| leg.flights.iter().cloned())
            .collect::<Vec<_>>()
            .join(",");
        numbers == wanted
    })
}

/// What the price did since it was parked, to the cent. `None` when either
/// end is unknown — an unknown movement must not read as no movement.
pub fn price_move(quoted: Option<f64>, now: Option<f64>) -> Option<f64> {
    let (quoted, now) = (quoted?, now?);
    Some(((now - quoted) * 100.0).round() / 100.0)
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --quiet trips::`
Expected: all pass, 2 new.

- [ ] **Step 5: Commit**

```bash
git add src/tools/trips.rs
git commit -m "feat: recognise a parked flight in fresh search results"
```

---

### Task 4: Totals, and refusing to invent one

**Files:**
- Modify: `src/tools/trips.rs`

- [ ] **Step 1: Define the output types**

These are used by this task's tests and again by Task 6, so they land here, before their first use.

```rust
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct PricedOption {
    pub airline: String,
    pub flight_numbers: String,
    pub itinerary: String,
    /// What it cost when it was parked. Never refreshed in the database —
    /// refreshing it would make the movement below shrink to nothing every
    /// time somebody looked.
    pub quoted_price: Option<f64>,
    pub quoted_currency: Option<String>,
    pub price_now: Option<f64>,
    pub moved: Option<f64>,
    /// False when this itinerary is not sold on that date any more.
    pub still_offered: bool,
}

#[derive(Debug, PartialEq, serde::Serialize)]
pub struct PricedSegment {
    pub position: i64,
    pub route: String,
    pub departure_date: String,
    pub chosen: PricedOption,
    /// The options that were not taken, priced from the same search. They
    /// cost nothing extra, and a decision made a week ago deserves checking
    /// against what the alternatives cost today.
    pub also_considered: Vec<PricedOption>,
    /// Sellers, when the provider offers any. Empty for Duffel-sourced
    /// segments while Duffel Links is disabled.
    pub booking: Vec<crate::tools::ignav::BookingOption>,
}

#[derive(Debug, PartialEq, serde::Serialize)]
pub struct FinalisedTrip {
    pub trip: String,
    pub adults: i64,
    pub segments: Vec<PricedSegment>,
    pub separate_total: Option<f64>,
    pub one_ticket_total: Option<f64>,
    pub currency: Option<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct FinaliseArgs {
    pub trip: String,
}
```

- [ ] **Step 2: Write the failing test**

```rust
fn priced(price: Option<f64>, currency: &str) -> PricedSegment {
    PricedSegment {
        position: 1,
        route: "AMS→LIS".to_string(),
        departure_date: "2026-09-03".to_string(),
        chosen: PricedOption {
            airline: "TAP".to_string(),
            flight_numbers: "TP675".to_string(),
            itinerary: "AMS 10:05 ✈ LIS 12:15".to_string(),
            quoted_price: Some(118.0),
            quoted_currency: Some(currency.to_string()),
            price_now: price,
            moved: None,
            still_offered: price.is_some(),
        },
        also_considered: Vec::new(),
        booking: Vec::new(),
    }
}

#[test]
fn a_total_is_only_produced_when_every_segment_agrees_on_a_currency() {
    let same = vec![priced(Some(131.0), "EUR"), priced(Some(200.0), "EUR")];
    assert_eq!(separate_total(&same), Ok((331.0, "EUR".to_string())));

    // Inventing a rate here would be the same class of error as doing
    // arithmetic on two local departure times.
    let mixed = vec![priced(Some(131.0), "EUR"), priced(Some(200.0), "USD")];
    let problem = separate_total(&mixed).unwrap_err();
    assert!(problem.contains("EUR"), "got: {problem}");
    assert!(problem.contains("USD"), "got: {problem}");

    // A segment nobody can price now has no total either.
    let gone = vec![priced(Some(131.0), "EUR"), priced(None, "EUR")];
    assert!(separate_total(&gone).is_err());
}

#[test]
fn a_separate_total_never_travels_without_what_it_costs_the_traveller() {
    // Four links is four tickets: bags collected and re-checked at every
    // join, and nobody obliged to rebook you when a leg is late.
    let notes = comparison_notes(3, Some(770.0), Some(812.0), "EUR");
    let joined = notes.join(" ");
    assert!(joined.contains("3 separate"), "got: {joined}");
    assert!(joined.to_lowercase().contains("late"), "got: {joined}");

    // And an absent comparison says why, rather than letting the separate
    // total look like a verdict.
    let notes = comparison_notes(3, Some(770.0), None, "EUR");
    let joined = notes.join(" ");
    assert!(joined.contains("could not"), "got: {joined}");
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test --quiet a_total_is_only_produced`
Expected: FAIL to compile — `cannot find function separate_total`.

- [ ] **Step 4: Implement**

```rust
/// The sum of the chosen options, or why there isn't one.
///
/// A total is only meaningful when every segment is priced and priced in the
/// same currency. Forcing one out of mixed currencies would need a rate
/// nobody supplied.
pub fn separate_total(segments: &[PricedSegment]) -> Result<(f64, String), String> {
    let mut currencies: Vec<String> = segments
        .iter()
        .filter_map(|s| s.chosen.quoted_currency.clone())
        .collect();
    currencies.sort();
    currencies.dedup();
    if currencies.len() > 1 {
        return Err(format!(
            "these segments are priced in {}, so there is no total to give without a \
             conversion rate nobody has supplied",
            currencies.join(" and ")
        ));
    }
    let currency = currencies.pop().ok_or_else(|| "no segment states a currency".to_string())?;

    let mut total = 0.0;
    for segment in segments {
        let price = segment.chosen.price_now.ok_or_else(|| {
            format!(
                "segment {} ({}) could not be priced now, so the trip has no total",
                segment.position, segment.route
            )
        })?;
        total += price;
    }
    Ok((((total * 100.0).round() / 100.0), currency))
}

/// What has to be said alongside the two totals.
///
/// These live in the tool's output rather than the preamble because the
/// booking fee already proved the difference: a disclosure the model is told
/// to make is one it sometimes doesn't.
pub fn comparison_notes(
    segment_count: usize,
    separate: Option<f64>,
    one_ticket: Option<f64>,
    currency: &str,
) -> Vec<String> {
    let mut notes = Vec::new();
    if separate.is_some() {
        notes.push(format!(
            "booking these links means {segment_count} separate tickets: bags are collected \
             and checked in again at every join, and if one leg runs late the next airline \
             is not obliged to wait or to rebook you"
        ));
    }
    match (separate, one_ticket) {
        (Some(sep), Some(one)) => {
            let gap = ((one - sep) * 100.0).round() / 100.0;
            notes.push(match gap > 0.0 {
                true => format!(
                    "one ticket costs {gap:.2} {currency} more than booking separately, and \
                     that is what the protection above is worth"
                ),
                false => format!(
                    "one ticket is {} {currency} cheaper than booking separately, and carries \
                     the protection as well",
                    gap.abs()
                ),
            });
        }
        (_, None) => notes.push(
            "the single-ticket comparison could not be made, so nothing here says whether \
             booking the whole trip on one ticket would be better — this is a missing \
             answer, not an argument for separate bookings"
                .to_string(),
        ),
        _ => {}
    }
    notes
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --quiet trips::`
Expected: all pass, 2 new.

- [ ] **Step 6: Commit**

```bash
git add src/tools/trips.rs
git commit -m "feat: total a trip only when its currencies agree"
```

---

### Task 5: Refusing before spending

**Files:**
- Modify: `src/tools/trips.rs`

Every one of these is checked before a single paid search, so a trip that cannot be priced costs nothing to discover.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn a_trip_that_cannot_be_priced_is_refused_before_anything_is_bought() {
    use crate::store::{TripSegment as Seg};

    let empty = vec![Seg {
        position: 1,
        origin: "AMS".into(),
        destination: "NRT".into(),
        departure_date: "2026-09-03".into(),
        candidates: vec![],
    }];
    let problem = ready_to_price(&empty).unwrap_err();
    assert!(problem.contains("segment 1"), "got: {problem}");
    assert!(problem.contains("no flight"), "got: {problem}");

    // Two options and no decision is a question, so the refusal asks it.
    let mut undecided = empty.clone();
    undecided[0].candidates = vec![
        TripCandidate { candidate: 1, chosen: false, ..parked("KL861", 940.0) },
        TripCandidate { candidate: 2, chosen: false, ..parked("CX270,CX500", 780.0) },
    ];
    let problem = ready_to_price(&undecided).unwrap_err();
    assert!(problem.contains("KL861"), "the options are listed: {problem}");
    assert!(problem.contains("CX270,CX500"), "got: {problem}");

    // One option and no decision is the pick by elimination. Demanding a
    // choice nobody has is ceremony.
    let mut lone = empty.clone();
    lone[0].candidates = vec![TripCandidate { candidate: 1, chosen: false, ..parked("KL861", 940.0) }];
    assert!(ready_to_price(&lone).is_ok());

    let mut decided = undecided.clone();
    decided[0].candidates[1].chosen = true;
    let chosen = ready_to_price(&decided).unwrap();
    assert_eq!(chosen.len(), 1);
    assert_eq!(chosen[0].1.flight_numbers, "CX270,CX500");
}

#[test]
fn a_trip_with_no_segments_is_refused() {
    assert!(ready_to_price(&[]).is_err());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet a_trip_that_cannot_be_priced`
Expected: FAIL to compile — `cannot find function ready_to_price`.

- [ ] **Step 3: Implement**

```rust
/// Each segment paired with the option that will be priced, or why the trip
/// is not ready.
///
/// A segment holding exactly one undecided option needs no decision: it is
/// the pick by elimination. Two or more without one is a question, so the
/// refusal lists them and asks it.
pub fn ready_to_price(
    segments: &[TripSegment],
) -> Result<Vec<(&TripSegment, &TripCandidate)>, String> {
    if segments.is_empty() {
        return Err("this trip has no segments yet, so there is nothing to price".to_string());
    }
    let mut ready = Vec::new();
    for segment in segments {
        let chosen = match segment.candidates.iter().find(|c| c.chosen) {
            Some(chosen) => chosen,
            None => match segment.candidates.as_slice() {
                [] => {
                    return Err(format!(
                        "segment {} ({}→{} on {}) has no flight on it yet — search that route \
                         and add one before pricing the trip",
                        segment.position,
                        segment.origin,
                        segment.destination,
                        segment.departure_date
                    ))
                }
                [only] => only,
                many => {
                    let options: Vec<String> = many
                        .iter()
                        .map(|c| format!("{} ({})", c.candidate, c.flight_numbers))
                        .collect();
                    return Err(format!(
                        "segment {} ({}→{}) still has {} options and none chosen: {}. \
                         Ask which one before pricing the trip.",
                        segment.position,
                        segment.origin,
                        segment.destination,
                        many.len(),
                        options.join(", ")
                    ));
                }
            },
        };
        ready.push((segment, chosen));
    }
    Ok(ready)
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --quiet trips::`
Expected: all pass, 2 new.

- [ ] **Step 5: Commit**

```bash
git add src/tools/trips.rs
git commit -m "feat: refuse to price a trip that is not decided yet"
```

---

### Task 6: The `finalise_trip` tool

**Files:**
- Modify: `src/tools/trips.rs`

- [ ] **Step 1: Write the failing test**

This one runs against wiremock, the way `duffel.rs` and `ignav.rs` already test providers.

```rust
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A Duffel offer-request response with one offer on the given flight
/// numbers, for the given total.
fn duffel_offer(total: &str, numbers: &[&str]) -> serde_json::Value {
    let segments: Vec<serde_json::Value> = numbers
        .iter()
        .map(|n| {
            serde_json::json!({
                "marketing_carrier_flight_number": n.trim_start_matches(char::is_alphabetic),
                "marketing_carrier": {"iata_code": &n[..2], "name": "KLM"},
                "origin": {"iata_code": "AMS"},
                "destination": {"iata_code": "NRT"},
                "departing_at": "2026-09-03T10:05:00",
                "arriving_at": "2026-09-03T12:15:00",
                "duration": "PT2H10M"
            })
        })
        .collect();
    serde_json::json!({"data": {"offers": [{
        "id": "off_fresh",
        "total_amount": total,
        "total_currency": "EUR",
        "owner": {"name": "KLM"},
        "slices": [{
            "origin": {"iata_code": "AMS"},
            "destination": {"iata_code": "NRT"},
            "duration": "PT2H10M",
            "segments": segments
        }]
    }]}})
}

#[tokio::test]
async fn finalising_prices_every_segment_and_says_what_moved() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/air/offer_requests"))
        .respond_with(ResponseTemplate::new(200).set_body_json(duffel_offer("980.00", &["KL861"])))
        .mount(&server)
        .await;

    let (store, _d) = setup();
    let shown = Arc::new(ShownFlights::default());
    shown.remember(99, vec![one_way("a", "AMS", "NRT", "2026-09-03", &["KL861"])], Instant::now());
    AddTripSegmentTool { store: store.clone(), user_id: 7 }
        .call(AddSegmentArgs {
            trip: "Japan".into(),
            origin: "AMS".into(),
            destination: "NRT".into(),
            departure_date: "2026-09-03".into(),
            position: None,
            adults: None,
            cabin_class: None,
        })
        .await
        .unwrap();
    AddTripOptionTool { store: store.clone(), user_id: 7, shown, chat_id: 99 }
        .call(AddOptionArgs {
            trip: "Japan".into(),
            position: 1,
            offer_id: "a".into(),
            decided: None,
        })
        .await
        .unwrap();

    let duffel = crate::tools::duffel::DuffelClient::new(
        reqwest::Client::new(),
        "test".to_string(),
        server.uri(),
    );
    let tool = FinaliseTripTool {
        store: store.clone(),
        user_id: 7,
        duffel: Some(duffel),
        ignav: None,
        budget: Arc::new(crate::tools::budget::FlightBudget::default()),
    };
    let out = tool.call(FinaliseArgs { trip: "Japan".into() }).await.unwrap();

    assert_eq!(out.segments.len(), 1);
    let chosen = &out.segments[0].chosen;
    assert_eq!(chosen.quoted_price, Some(940.0), "what it cost when parked");
    assert_eq!(chosen.price_now, Some(980.0));
    assert_eq!(chosen.moved, Some(40.0));
    assert!(chosen.still_offered);
    // One segment, so there is no single-ticket comparison to make, and the
    // output has to say so rather than let the separate total stand alone.
    assert!(out.one_ticket_total.is_none());
    assert!(out.notes.iter().any(|n| n.contains("could not")), "notes: {:?}", out.notes);

    // Priced means priced: the trip records it, and the parked price is
    // left exactly as it was.
    let trip = store.find_trip(7, "Japan").unwrap().unwrap();
    assert_eq!(trip.status, "finalised");
    assert_eq!(trip.segments[0].candidates[0].quoted_price, Some(940.0));
}

#[tokio::test]
async fn a_flight_that_is_no_longer_sold_is_reported_not_substituted() {
    // The traveller chose a flight, not a price band. Quietly swapping it
    // is how a 06:00 departure turns up in an itinerary nobody agreed to.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/air/offer_requests"))
        .respond_with(ResponseTemplate::new(200).set_body_json(duffel_offer("640.00", &["KL999"])))
        .mount(&server)
        .await;

    let (store, _d) = setup();
    let shown = Arc::new(ShownFlights::default());
    shown.remember(99, vec![one_way("a", "AMS", "NRT", "2026-09-03", &["KL861"])], Instant::now());
    AddTripSegmentTool { store: store.clone(), user_id: 7 }
        .call(AddSegmentArgs {
            trip: "Japan".into(),
            origin: "AMS".into(),
            destination: "NRT".into(),
            departure_date: "2026-09-03".into(),
            position: None,
            adults: None,
            cabin_class: None,
        })
        .await
        .unwrap();
    AddTripOptionTool { store: store.clone(), user_id: 7, shown, chat_id: 99 }
        .call(AddOptionArgs { trip: "Japan".into(), position: 1, offer_id: "a".into(), decided: None })
        .await
        .unwrap();

    let duffel = crate::tools::duffel::DuffelClient::new(
        reqwest::Client::new(),
        "test".to_string(),
        server.uri(),
    );
    let out = FinaliseTripTool {
        store,
        user_id: 7,
        duffel: Some(duffel),
        ignav: None,
        budget: Arc::new(crate::tools::budget::FlightBudget::default()),
    }
    .call(FinaliseArgs { trip: "Japan".into() })
    .await
    .unwrap();

    let chosen = &out.segments[0].chosen;
    assert!(!chosen.still_offered);
    assert_eq!(chosen.price_now, None);
    assert_eq!(chosen.flight_numbers, "KL861", "still says what they picked");
    assert!(out.separate_total.is_none(), "no total when a segment cannot be priced");
    assert!(
        out.notes.iter().any(|n| n.contains("KL861")),
        "the traveller has to be told: {:?}",
        out.notes
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet finalising_prices_every_segment`
Expected: FAIL to compile — `cannot find struct FinaliseTripTool`.

- [ ] **Step 3: Implement the tool**

```rust
use crate::tools::budget::FlightBudget;
use crate::tools::duffel::{
    merged_search, DuffelClient, FlightQuery, MultiCityQuery, Slice,
};
use crate::tools::ignav::IgnavClient;

pub struct FinaliseTripTool {
    pub store: Store,
    pub user_id: i64,
    pub duffel: Option<DuffelClient>,
    pub ignav: Option<IgnavClient>,
    pub budget: Arc<FlightBudget>,
}

impl Tool for FinaliseTripTool {
    const NAME: &'static str = "finalise_trip";
    type Error = StoreToolError;
    type Args = FinaliseArgs;
    type Output = FinalisedTrip;

    fn description(&self) -> String {
        "Price a finished trip: every segment is searched again for today's \
         price and where to buy it, and the whole itinerary is also priced as \
         a single ticket so the two can be compared. Call this only when every \
         segment has a flight decided. Prices stored on a trip are stale by \
         construction — this is the only thing that produces current ones."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {"trip": {"type": "string", "description": "the trip's name"}},
            "required": ["trip"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let store = self.store.clone();
        let user_id = self.user_id;
        let name = args.trip.clone();
        let trip = tokio::task::spawn_blocking(move || store.find_trip(user_id, &name))
            .await
            .map_err(internal)?
            .map_err(internal)?
            .ok_or_else(|| StoreToolError(format!("no trip called {:?}", args.trip)))?;

        // Everything that can refuse, refuses before a single paid search.
        let ready = ready_to_price(&trip.segments).map_err(StoreToolError)?;
        dates_run_forwards(&trip.segments).map_err(StoreToolError)?;
        self.budget.grant_trip(trip.segments.len());

        let adults = u32::try_from(trip.adults).unwrap_or(1).max(1);
        let cabin = trip.cabin_class.clone();

        // Each segment's re-price, and the whole-trip request, at once.
        let per_segment = futures::future::join_all(ready.iter().map(|(segment, chosen)| {
            let day = FlightQuery {
                origin: segment.origin.clone(),
                destination: segment.destination.clone(),
                departure_date: segment.departure_date.clone(),
                return_date: None,
                adults: Some(adults),
                cabin_class: cabin.clone(),
                max_connections: None,
                flex_days: None,
            };
            async move {
                if !self.budget.claim_one() {
                    return (*segment, *chosen, Vec::new());
                }
                let offers =
                    merged_search(self.duffel.as_ref(), self.ignav.as_ref(), &day).await;
                let offers = offers.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, position = segment.position, "segment re-price failed");
                    Vec::new()
                });
                (*segment, *chosen, offers)
            }
        }));

        let one_ticket = async {
            let duffel = self.duffel.as_ref()?;
            if trip.segments.len() < 2 || !self.budget.claim_one() {
                return None;
            }
            let query = MultiCityQuery {
                slices: trip
                    .segments
                    .iter()
                    .map(|s| Slice::new(&s.origin, &s.destination, &s.departure_date))
                    .collect(),
                adults,
                cabin_class: cabin.clone(),
            };
            match duffel.search_multi_city(&query).await {
                Ok(offers) => offers
                    .iter()
                    .map(|o| o.price)
                    .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)),
                Err(e) => {
                    tracing::warn!(error = %e, "single-ticket pricing failed");
                    None
                }
            }
        };

        let (segment_results, one_ticket_total) =
            futures::future::join(per_segment, one_ticket).await;

        let mut segments = Vec::new();
        let mut notes = Vec::new();
        for (segment, chosen, offers) in segment_results {
            let priced = price_option(chosen, &offers);
            if !priced.still_offered {
                notes.push(format!(
                    "segment {} ({}→{}): {} is not sold on {} any more, so this trip has no \
                     current total. Search that route again and pick a replacement.",
                    segment.position,
                    segment.origin,
                    segment.destination,
                    chosen.flight_numbers,
                    segment.departure_date
                ));
            }
            let also_considered = segment
                .candidates
                .iter()
                .filter(|c| c.candidate != chosen.candidate)
                .map(|c| price_option(c, &offers))
                .collect();
            let booking = match match_candidate(chosen, &offers) {
                Some(offer) => self.sellers(offer).await,
                None => Vec::new(),
            };
            segments.push(PricedSegment {
                position: segment.position,
                route: format!("{}→{}", segment.origin, segment.destination),
                departure_date: segment.departure_date.clone(),
                chosen: priced,
                also_considered,
                booking,
            });
        }

        let (separate_total, currency) = match separate_total(&segments) {
            Ok((total, currency)) => (Some(total), Some(currency)),
            Err(problem) => {
                notes.push(problem);
                (None, None)
            }
        };
        notes.extend(comparison_notes(
            segments.len(),
            separate_total,
            one_ticket_total,
            currency.as_deref().unwrap_or(""),
        ));
        if self.duffel.is_none() {
            notes.push(
                "Duffel is not configured here, so no single-ticket price could be fetched at \
                 all — this trip can only be compared against itself"
                    .to_string(),
            );
        }

        let store = self.store.clone();
        let trip_id = trip.id;
        if let Err(e) =
            tokio::task::spawn_blocking(move || store.set_trip_status(trip_id, "finalised")).await
        {
            tracing::warn!(error = %e, "could not record that a trip was finalised");
        }

        Ok(FinalisedTrip {
            trip: trip.name,
            adults: trip.adults,
            segments,
            separate_total,
            one_ticket_total,
            currency,
            notes,
        })
    }
}

impl FinaliseTripTool {
    /// Where a matched offer can actually be bought. Ignav resolves its own
    /// ids to sellers; a Duffel offer has nowhere to send anyone while
    /// Duffel Links is disabled, so it comes back empty and the reply says
    /// to book with the airline.
    async fn sellers(&self, offer: &Flight) -> Vec<crate::tools::ignav::BookingOption> {
        let (Some(ignav), Source::Ignav) = (self.ignav.as_ref(), offer.source) else {
            return Vec::new();
        };
        match ignav.booking_links(&offer.offer_id).await {
            Ok(links) => links.options,
            Err(e) => {
                tracing::warn!(error = %e, "booking links failed for a finalised segment");
                Vec::new()
            }
        }
    }
}

/// One candidate against today's offers.
fn price_option(candidate: &TripCandidate, offers: &[Flight]) -> PricedOption {
    let found = match_candidate(candidate, offers);
    let price_now = found.map(|f| f.price);
    PricedOption {
        airline: candidate.airline.clone(),
        flight_numbers: candidate.flight_numbers.clone(),
        itinerary: candidate.itinerary.clone(),
        quoted_price: candidate.quoted_price,
        quoted_currency: candidate.quoted_currency.clone(),
        price_now,
        moved: price_move(candidate.quoted_price, price_now),
        still_offered: found.is_some(),
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --quiet trips::`
Expected: all pass, 2 new.

- [ ] **Step 5: Commit**

```bash
git add src/tools/trips.rs
git commit -m "feat: finalise a trip by re-pricing it against one ticket"
```

---

### Task 7: Register the tool

**Files:**
- Modify: `src/agent.rs`

- [ ] **Step 1: Register it beside the flight tools**

`finalise_trip` needs a provider to be useful, so it goes inside the same block that registers `FlightSearchTool` — the one guarded by `d.duffel.is_some() || d.ignav.is_some()` at `src/agent.rs:595`.

That block currently **moves** the allowance into the search tool with `budget: flights,`. Both tools must share one allowance, or a request that searches and then finalises would draw on two and the cap would mean nothing. So first change that line to clone:

```rust
            budget: flights.clone(),
```

Then add, after the `FlightSearchTool` registration inside the same `if` block:

```rust
        builder = builder.tool(crate::tools::trips::FinaliseTripTool {
            store: d.store.clone(),
            user_id,
            duffel: d.duffel.clone(),
            ignav: d.ignav.clone(),
            // The same allowance the search tool got: one request, one cap.
            budget: flights.clone(),
        });
```

Note the `ignav` field here takes `d.ignav.clone()` directly, unlike `FlightSearchTool`, which wraps it in a per-user market. Finalisation resolves booking links by id, and Ignav rejects a market sent alongside an id — that was measured as a 400 (`conflicting_booking_lookup_mode`).

- [ ] **Step 2: Add preamble guidance**

Append to the trips paragraph added in the building plan:

```
Finalising is the only thing that produces current prices, and it costs a
search per segment, so call it when the trip is settled rather than to check
on it. Present both totals it returns and never drop the note about separate
tickets: a link per segment is a ticket per segment, and the traveller is
carrying the risk at every join. If the single-ticket total is missing, say
that it is missing — it is not evidence that separate booking is better.
```

- [ ] **Step 3: Verify everything**

Run: `cargo test --quiet && cargo clippy --all-targets --quiet`
Expected: all tests pass; clippy silent.

Do not run `cargo fmt` — this repo is not rustfmt-formatted and it would rewrite every file.

- [ ] **Step 4: Commit**

```bash
git add src/agent.rs
git commit -m "feat: give the agent finalise_trip"
```

---

## Done when

- `cargo test` passes and `cargo clippy --all-targets` is silent
- A trip with several segments can be finalised, giving per-segment prices now against what was parked, the runners-up priced too, a separate total and a single-ticket total
- A segment whose flight is gone is reported and never substituted
- A missing single-ticket price is stated as missing
- A mixed-currency trip gets no total and says why
- Finalising leaves every `quoted_price` in the database untouched
