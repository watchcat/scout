//! Building a trip: a named plan the traveller assembles over as many
//! messages as it takes.
//!
//! Nothing here stores an offer id. Offers expire in minutes and a plan
//! outlives the conversation that made it, so a trip holds the itinerary —
//! airports, dates, flight numbers — and finalisation re-prices it.

use crate::store::{ExpectedSegment, NewCandidate, Store, Trip, TripCandidate, TripSegment};
use crate::tools::budget::FlightBudget;
use crate::tools::duffel::{merged_search, DuffelClient, Flight, FlightQuery, MultiCityQuery, Slice, Source};
use crate::tools::ignav::IgnavClient;
use crate::tools::purchases::{internal, StoreToolError};
use crate::tools::shown::ShownFlights;
use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;

/// Less than this between landing and the next departure is the shape of an
/// itinerary somebody misses.
const TIGHT_TURNAROUND_MINUTES: i64 = 180;

/// What is worth saying about an itinerary without refusing it.
///
/// Both of these are legitimate trips, so they are notes rather than
/// errors. Silence would be the actual failure: an itinerary that reads as
/// continuous when it is not is one somebody plans around.
pub fn itinerary_notes(segments: &[TripSegment]) -> Vec<String> {
    let mut notes = Vec::new();
    for pair in segments.windows(2) {
        let (before, after) = (&pair[0], &pair[1]);
        if before.destination != after.origin {
            notes.push(format!(
                "segment {} arrives at {} and segment {} leaves from {} — getting between \
                 them is not part of this trip",
                before.position, before.destination, after.position, after.origin
            ));
            // Two clocks in two places. This codebase does not subtract
            // those, so the gap note is all there is to say.
            continue;
        }
        if let Some(minutes) = turnaround_minutes(before, after) {
            if minutes < 0 {
                // Same airport, so these times are comparable, and the next
                // departure is before the previous arrival. No schedule
                // makes that flyable — say so plainly rather than as a
                // negative number of minutes, which would read as a typo.
                notes.push(format!(
                    "segment {} is scheduled to leave before segment {} lands — this \
                     itinerary cannot be flown as written",
                    after.position, before.position
                ));
            } else if minutes < TIGHT_TURNAROUND_MINUTES {
                notes.push(format!(
                    "only {}h {:02}m at {} between segment {} landing and segment {} leaving",
                    minutes / 60,
                    minutes % 60,
                    before.destination,
                    before.position,
                    after.position
                ));
            }
        }
    }
    notes
}

/// The wait between one segment landing and the next leaving, when both are
/// known and at the same airport.
///
/// Same airport is what makes this legitimate: both timestamps are local to
/// that one place, so unlike the two ends of a leg they really are
/// comparable. See `Connection`, which relies on the same fact.
fn turnaround_minutes(before: &TripSegment, after: &TripSegment) -> Option<i64> {
    let parse = |s: &String| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok();
    let landed = parse(taken(before)?.arriving_at_local.as_ref()?)?;
    let leaves = parse(taken(after)?.departing_at_local.as_ref()?)?;
    Some((leaves - landed).num_minutes())
}

/// The option a segment is going with: the chosen one, or the only one
/// there is. Matches what `ready_to_price` will accept, so the warning and
/// the pricing never disagree about which flight is meant.
fn taken(segment: &TripSegment) -> Option<&TripCandidate> {
    segment
        .candidates
        .iter()
        .find(|c| c.chosen)
        .or(match segment.candidates.as_slice() {
            [only] => Some(only),
            _ => None,
        })
}

/// Dates must run forwards before an itinerary can be priced as one ticket.
/// Equal dates are fine: a same-day connection is an ordinary thing.
///
/// Assumes every `departure_date` is zero-padded `YYYY-MM-DD` — the one
/// shape under which comparing them as text agrees with comparing them as
/// dates. That is enforced at the tool boundary before a segment is ever
/// stored, not here, so a caller that bypasses it gets a wrong answer
/// instead of an error.
pub fn dates_run_forwards(segments: &[TripSegment]) -> Result<(), String> {
    for pair in segments.windows(2) {
        // ISO dates compare correctly as text, which is the one place that
        // is true of them.
        if pair[1].departure_date.trim() < pair[0].departure_date.trim() {
            return Err(format!(
                "segment {} leaves on {} but segment {} leaves on {}, which is earlier — \
                 fix the dates or the order before pricing this",
                pair[0].position,
                pair[0].departure_date,
                pair[1].position,
                pair[1].departure_date
            ));
        }
    }
    Ok(())
}

/// What every trip tool hands back: the trip in full, plus anything worth
/// saying about it. The notes travel in the output rather than the preamble
/// for the reason the booking fee taught — a disclosure the model is *told*
/// to make is a disclosure it sometimes doesn't.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct TripView {
    pub trip: Trip,
    pub notes: Vec<String>,
}

impl TripView {
    fn of(trip: Trip) -> Self {
        let notes = itinerary_notes(&trip.segments);
        Self { trip, notes }
    }
}

/// Duffel and Ignav both take IATA codes only; "Amsterdam" is a 422 and a
/// wasted search fee, so it is rejected at the point it is typed.
fn iata(label: &str, value: &str) -> Result<String, StoreToolError> {
    let code = value.trim();
    match code.len() == 3 && code.chars().all(|c| c.is_ascii_alphabetic()) {
        true => Ok(code.to_ascii_uppercase()),
        false => Err(StoreToolError(format!(
            "{label} must be a 3-letter IATA airport or city code (AMS, LHR, NYC), not {value:?} \
             — use the code for the place the traveller named"
        ))),
    }
}

/// Reformats through the parsed date rather than returning the trimmed
/// input: `chrono` accepts "2026-9-3", but `dates_run_forwards` compares
/// `departure_date` as text, which only agrees with date order when every
/// date is zero-padded. This is the one place that padding is established.
fn calendar_date(value: &str) -> Result<String, StoreToolError> {
    let date = value.trim();
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|d| d.format("%Y-%m-%d").to_string())
        .map_err(|_| StoreToolError(format!("departure_date must be YYYY-MM-DD, not {value:?}")))
}

/// The offer in `offers` that is the same itinerary as `candidate`, if it is
/// still sold.
///
/// Flight numbers in order are the identity: they are what survives an offer
/// expiring, which is the whole reason a trip stores them instead of an id.
/// Order is part of it — the same two numbers flown the other way round is a
/// different journey.
pub fn match_candidate<'a>(candidate: &TripCandidate, offers: &'a [Flight]) -> Option<&'a Flight> {
    let wanted = candidate.flight_numbers.trim();
    offers.iter().find(|offer| match offer.legs.as_slice() {
        // A stored candidate can only ever have come from a single leg —
        // add_trip_option refuses to bind a return offer to a segment — so
        // this is symmetric with how the data was created, not an extra
        // restriction. Without it, a two-leg return with one flight number
        // per leg joins to the same string as a genuine one-leg, two-flight
        // connection and the two become indistinguishable.
        [leg] => leg.flights.join(",") == wanted,
        _ => false,
    })
}

/// What the price did since it was parked, to the cent. `None` when either
/// end is unknown — an unknown movement must not read as no movement.
pub fn price_move(quoted: Option<f64>, now: Option<f64>) -> Option<f64> {
    let (quoted, now) = (quoted?, now?);
    Some(((now - quoted) * 100.0).round() / 100.0)
}

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
    /// The currency `price_now` is actually stated in. Never assumed to be
    /// `quoted_currency`: that is what this itinerary cost when it was
    /// parked, and finalisation exists precisely because that figure is
    /// stale. A total summed from `price_now` has to be labelled from this
    /// field, not that one.
    pub price_now_currency: Option<String>,
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

/// The sum of the chosen options, or why there isn't one.
///
/// A total is only meaningful when every segment is priced *now*, and every
/// one of those current prices agrees on a currency. Agreement is checked
/// against `price_now_currency`, never `quoted_currency`: the parked
/// currency is what an itinerary cost before finalisation re-priced it, and
/// summing today's numbers under yesterday's label is the exact laundering
/// this function exists to refuse — a trip parked all in EUR does not mean
/// its live prices still are. Forcing a total out of mixed currencies, or
/// out of a price with no stated currency of its own, would need a
/// conversion rate nobody supplied.
pub fn separate_total(segments: &[PricedSegment]) -> Result<(f64, String), String> {
    let mut currencies: Vec<String> = Vec::new();
    let mut total = 0.0;
    for segment in segments {
        let price = segment.chosen.price_now.ok_or_else(|| {
            format!(
                "segment {} ({}) could not be priced now, so the trip has no total",
                segment.position, segment.route
            )
        })?;
        let currency = segment.chosen.price_now_currency.clone().ok_or_else(|| {
            format!(
                "segment {} ({}) has a current price but no stated currency, so it cannot be \
                 added to a total",
                segment.position, segment.route
            )
        })?;
        currencies.push(currency);
        total += price;
    }
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
            // The currency code is dropped rather than printed empty:
            // "42.00  more" (double space, no code) reads like a rendering
            // bug and undermines trust in the number sitting right next to
            // it. The caller only ever passes "" when no currency could be
            // determined, so there is nothing correct to print anyway.
            let amount = match currency.is_empty() {
                true => format!("{:.2}", gap.abs()),
                false => format!("{:.2} {currency}", gap.abs()),
            };
            notes.push(match gap > 0.0 {
                true => format!(
                    "one ticket costs {amount} more than booking separately, and that is \
                     what the protection above is worth"
                ),
                false => format!(
                    "one ticket is {amount} cheaper than booking separately, and carries \
                     the protection as well"
                ),
            });
        }
        // A missing comparison must never read as a verdict either way —
        // that silence is exactly what would cost somebody money, whichever
        // side turns out to be the one that is actually missing.
        (Some(_), None) => notes.push(
            "the single-ticket comparison could not be made, so nothing here says whether \
             booking the whole trip on one ticket would be better — this is a missing \
             answer, not an argument for separate bookings"
                .to_string(),
        ),
        (None, Some(_)) => notes.push(
            "the separate-booking total could not be given, so nothing here says whether \
             booking each segment apart would be better — this is a missing answer, not an \
             argument for the single ticket"
                .to_string(),
        ),
        (None, None) => {}
    }
    notes
}

/// Each segment paired with the option that will be priced, or why the trip
/// is not ready.
///
/// A segment holding exactly one undecided option needs no decision: it is
/// the pick by elimination. Two or more without one is a question, so the
/// refusal lists them and asks it.
pub fn ready_to_price(segments: &[TripSegment]) -> Result<Vec<(&TripSegment, &TripCandidate)>, String> {
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
                        segment.position, segment.origin, segment.destination, segment.departure_date
                    ))
                }
                [only] => only,
                many => {
                    let options: Vec<String> =
                        many.iter().map(|c| format!("{} ({})", c.candidate, c.flight_numbers)).collect();
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

/// One candidate against today's offers.
///
/// `price_now_currency` comes from the matched offer's own `currency`, never
/// from `quoted_currency` — see `separate_total`'s comment for why that
/// distinction is the whole point.
fn price_option(candidate: &TripCandidate, offers: &[Flight]) -> PricedOption {
    let found = match_candidate(candidate, offers);
    PricedOption {
        airline: candidate.airline.clone(),
        flight_numbers: candidate.flight_numbers.clone(),
        itinerary: candidate.itinerary.clone(),
        quoted_price: candidate.quoted_price,
        quoted_currency: candidate.quoted_currency.clone(),
        price_now: found.map(|f| f.price),
        price_now_currency: found.map(|f| f.currency.clone()),
        moved: price_move(candidate.quoted_price, found.map(|f| f.price)),
        still_offered: found.is_some(),
    }
}

/// Re-prices a built trip: every segment against today's market, and the
/// whole itinerary against Duffel as a single ticket.
///
/// This is a re-pricing pass, never a checkout — a trip stores an itinerary,
/// not an offer, because offers expire in minutes and a plan outlives the
/// conversation that made it. Nothing here is bookable; everything quoted is
/// fetched fresh, on every call.
pub struct FinaliseTripTool {
    pub store: Store,
    pub user_id: i64,
    pub duffel: Option<DuffelClient>,
    /// Takes the id straight from `AgentDeps`, not the per-user market
    /// wrapper `FlightSearchTool` uses: booking links are resolved by
    /// `ignav_id`, and Ignav rejects a market sent alongside one (measured:
    /// a 400 `conflicting_booking_lookup_mode`).
    pub ignav: Option<IgnavClient>,
    /// Shared with `FlightSearchTool` so one user request draws on one cap,
    /// not two.
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
        let trip = tokio::task::spawn_blocking(move || find_trip_or_list(&store, user_id, &name))
            .await
            .map_err(internal)?
            .map_err(internal)?;

        // Everything that can refuse, refuses before a single paid search: a
        // trip that cannot be priced must cost nothing to discover.
        let ready = ready_to_price(&trip.segments).map_err(StoreToolError)?;
        dates_run_forwards(&trip.segments).map_err(StoreToolError)?;
        self.budget.grant_trip(trip.segments.len());

        let adults = u32::try_from(trip.adults).unwrap_or(1).max(1);
        let cabin = trip.cabin_class.clone();

        // Each segment's re-price, and the whole-trip request, at once —
        // sequentially this would be several seconds of silence for no
        // reason.
        let per_segment = futures::future::join_all(ready.iter().map(|(segment, chosen)| {
            // Owned, not borrowed: these outlive `trip` inside the async
            // block below, and `TripSegment`/`TripCandidate` are cheap to
            // clone next to a network round trip.
            let segment = (*segment).clone();
            let chosen = (*chosen).clone();
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
                    return (segment, chosen, Vec::new());
                }
                let offers = merged_search(self.duffel.as_ref(), self.ignav.as_ref(), &day).await;
                let offers = offers.unwrap_or_else(|e| {
                    tracing::warn!(
                        error = %e,
                        position = segment.position,
                        "segment re-price failed"
                    );
                    Vec::new()
                });
                (segment, chosen, offers)
            }
        }));

        let one_ticket = async {
            let duffel = self.duffel.as_ref()?;
            // A single slice is an ordinary search, not a comparison — and
            // the budget was only ever granted for a real multi-city request.
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

        let (segment_results, one_ticket_total) = futures::future::join(per_segment, one_ticket).await;

        let mut segments = Vec::new();
        let mut notes = Vec::new();
        for (segment, chosen, offers) in segment_results {
            let priced = price_option(&chosen, &offers);
            if !priced.still_offered {
                // The traveller chose a flight, not a price band: reported,
                // never quietly substituted for something else that matched
                // the route.
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
            // Runners-up come free with the same search: a decision made a
            // week ago deserves checking against what the alternatives cost
            // today.
            let also_considered = segment
                .candidates
                .iter()
                .filter(|c| c.candidate != chosen.candidate)
                .map(|c| price_option(c, &offers))
                .collect();
            let booking = match match_candidate(&chosen, &offers) {
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

        // quoted_price and quoted_at are never touched here — see
        // TripCandidate's own comment. Only the status changes, to record
        // that this trip has been priced.
        let store = self.store.clone();
        let trip_id = trip.id;
        match tokio::task::spawn_blocking(move || store.set_trip_status(trip_id, "finalised")).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "could not record that a trip was finalised"),
            Err(e) => tracing::warn!(error = %e, "could not record that a trip was finalised"),
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
    /// ids to sellers; a Duffel offer has nowhere to send anyone while Duffel
    /// Links is disabled, so it comes back empty and the reply says to book
    /// with the airline instead.
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

#[derive(Debug, Deserialize)]
pub struct AddSegmentArgs {
    pub trip: String,
    pub origin: String,
    pub destination: String,
    pub departure_date: String,
    #[serde(default)]
    pub position: Option<i64>,
    #[serde(default)]
    pub adults: Option<i64>,
    #[serde(default)]
    pub cabin_class: Option<String>,
}

pub struct AddTripSegmentTool {
    pub store: Store,
    pub user_id: i64,
}

impl Tool for AddTripSegmentTool {
    const NAME: &'static str = "add_trip_segment";
    type Error = StoreToolError;
    type Args = AddSegmentArgs;
    type Output = TripView;

    fn description(&self) -> String {
        "Add one flight leg to a named trip, creating the trip if that name is \
         new. Use this to build a multi-city itinerary the traveller is \
         planning across several messages. A segment is one direction on one \
         date: a return is two segments. Airports are 3-letter IATA codes, \
         dates are YYYY-MM-DD. Segments are numbered from 1 in travel order; \
         pass position to insert rather than append."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "trip": {"type": "string", "description": "what the traveller calls this trip; a new name starts a new trip"},
                "origin": {"type": "string", "description": "3-letter IATA code"},
                "destination": {"type": "string", "description": "3-letter IATA code"},
                "departure_date": {"type": "string", "description": "YYYY-MM-DD"},
                "position": {"type": "integer", "description": "insert at this 1-based position; omit to append"},
                "adults": {"type": "integer", "description": "passengers for the whole trip; 1 on a new trip, otherwise left as it was unless given"},
                "cabin_class": {"type": "string", "description": "economy, premium_economy, business or first, for the whole trip"}
            },
            "required": ["trip", "origin", "destination", "departure_date"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        // Validated before anything is written, so a mistyped code cannot
        // leave a half-built trip behind.
        let origin = iata("origin", &args.origin)?;
        let destination = iata("destination", &args.destination)?;
        if origin == destination {
            return Err(StoreToolError(format!(
                "origin and destination are both {origin}; a flight needs two different places"
            )));
        }
        let date = calendar_date(&args.departure_date)?;

        let store = self.store.clone();
        let user_id = self.user_id;
        tokio::task::spawn_blocking(move || -> anyhow::Result<Trip> {
            let trip = store.upsert_trip(
                user_id,
                &args.trip,
                args.adults,
                args.cabin_class.as_deref(),
            )?;
            store
                .add_segment(trip.id, args.position, &origin, &destination, &date)
                .map_err(lost_trip_race)
        })
        .await
        .map_err(internal)?
        .map(TripView::of)
        .map_err(internal)
    }
}

/// A trip lookup and the write that follows it are two separate lock
/// acquisitions in every one of these tools, so a concurrent `delete_trip`
/// landing between them makes the write fail with a bare "no such trip" —
/// on a tool that just confirmed the trip exists, which reads as nonsense.
/// Nothing was written either way, so retrying is exactly the right advice;
/// only the wording needed fixing. Shared by every tool with this shape so
/// the same race reads the same way wherever it turns up.
fn lost_trip_race(e: anyhow::Error) -> anyhow::Error {
    match e.to_string().as_str() {
        "no such trip" => anyhow::anyhow!(
            "the trip was deleted while this change was being made — nothing was written, try again"
        ),
        _ => e,
    }
}

/// "none yet", or the traveller's trip names joined for a human to read.
fn trip_names(store: &Store, user_id: i64) -> anyhow::Result<String> {
    let names: Vec<String> = store.list_trips(user_id)?.into_iter().map(|t| t.name).collect();
    Ok(match names.is_empty() {
        true => "none yet".to_string(),
        false => names.join(", "),
    })
}

/// The named trip, or an error naming the trips that do exist.
///
/// The list is the point: a model that mistypes a name can correct itself
/// from the reply instead of spending another call finding out, or asking
/// the traveller a question it could have answered.
fn find_trip_or_list(store: &Store, user_id: i64, name: &str) -> anyhow::Result<Trip> {
    match store.find_trip(user_id, name)? {
        Some(trip) => Ok(trip),
        None => anyhow::bail!("no trip called {name:?}. This traveller has: {}", trip_names(store, user_id)?),
    }
}

/// Turns a shown flight into something durable. Everything perishable is
/// dropped here, the offer id above all: it is the one field that looks
/// useful and is guaranteed to be wrong later.
fn candidate_from(flight: &Flight) -> NewCandidate {
    NewCandidate {
        airline: flight.airline.clone(),
        flight_numbers: flight
            .legs
            .iter()
            .flat_map(|leg| leg.flights.iter().cloned())
            .collect::<Vec<_>>()
            .join(","),
        itinerary: flight
            .legs
            .iter()
            .map(|leg| leg.itinerary.clone())
            .collect::<Vec<_>>()
            .join(" / "),
        departing_at_local: flight.legs.first().map(|l| l.departing_at_local.clone()),
        arriving_at_local: flight.legs.last().map(|l| l.arriving_at_local.clone()),
        duration_minutes: flight.total_minutes.map(i64::from),
        quoted_price: Some(flight.price),
        quoted_currency: Some(flight.currency.clone()),
        source: Some(
            match flight.source {
                Source::Duffel => "duffel",
                Source::Ignav => "ignav",
            }
            .to_string(),
        ),
    }
}

#[derive(Debug, Deserialize)]
pub struct AddOptionArgs {
    pub trip: String,
    pub position: i64,
    pub offer_id: String,
    /// Defaults to true: "this is the flight" must not need a second call.
    #[serde(default)]
    pub decided: Option<bool>,
}

pub struct AddTripOptionTool {
    pub store: Store,
    pub user_id: i64,
    /// What this chat was shown, so an invented id costs nothing to refuse.
    pub shown: Arc<ShownFlights>,
    pub chat_id: i64,
}

impl Tool for AddTripOptionTool {
    const NAME: &'static str = "add_trip_option";
    type Error = StoreToolError;
    type Args = AddOptionArgs;
    type Output = TripView;

    fn description(&self) -> String {
        "Attach a flight from a recent search_flights result to one segment of \
         a trip. Pass decided=false to keep it as an option the traveller has \
         not settled on yet — several options may sit on one segment, and \
         choose_trip_option picks between them later. The offer_id must come \
         from a search in this conversation."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "trip": {"type": "string", "description": "the trip's name"},
                "position": {"type": "integer", "description": "which segment, 1-based"},
                "offer_id": {"type": "string", "description": "offer_id from a search_flights result in this conversation"},
                "decided": {"type": "boolean", "description": "false to park it as an undecided option; default true"}
            },
            "required": ["trip", "position", "offer_id"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        // One instant for both the lookup and its error path, as
        // `BookingLinksTool` does — two calls a few instructions apart could
        // in principle straddle the expiry, and answering "not shown" while
        // also listing the very id in `offer_ids` would be worse than
        // either answer alone.
        let now = Instant::now();

        // Guard 1: the offer must be one this chat was actually shown. A
        // model carrying a 32-character id across a turn is the invented-id
        // problem again, and a made-up one otherwise costs a paid lookup to
        // discover. Chat-scoped, too — otherwise one household member could
        // bind a flight from another's search.
        let flight = self.shown.find(self.chat_id, &args.offer_id, now).ok_or_else(|| {
            let ids = self.shown.offer_ids(self.chat_id, now);
            StoreToolError(format!(
                "{:?} was not shown in this conversation, so it cannot be added. \
                 Search first, then use one of these offer_ids: {}",
                args.offer_id,
                match ids.is_empty() {
                    true => "(nothing has been searched yet)".to_string(),
                    false => ids.join(", "),
                }
            ))
        })?;

        // Guard 2: a segment is one direction on one date. A return offer
        // has two legs, and binding it here would price the return twice
        // and misdescribe the trip.
        if flight.legs.len() != 1 {
            return Err(StoreToolError(format!(
                "that offer is a return with {} legs, and a trip segment is one direction. \
                 Add each direction as its own segment and pick a one-way for each.",
                flight.legs.len()
            )));
        }
        let leg = &flight.legs[0];
        let candidate = candidate_from(&flight);

        // Guard 3 (route, and now date) is enforced inside
        // `Store::add_candidate`, in the same lock acquisition as the
        // insert — checking it here, against a `Trip` read a moment
        // earlier, would leave a window for a concurrent add_trip_segment
        // or drop_trip_segment to renumber positions in between. What is
        // decided here is only whether there is a date to check at all:
        // `departing_at_local` can be an empty string — Duffel's parser
        // falls back to one when an offer's leg states no departure time —
        // and that is "nothing to check", not "checked and fine", so it
        // must not be silently skipped or wrongly treated as a mismatch.
        let leg_date = leg.departing_at_local.get(0..10).map(str::to_string);
        let date_unchecked = leg_date.is_none();

        let store = self.store.clone();
        let user_id = self.user_id;
        let (leg_origin, leg_destination) = (leg.origin.clone(), leg.destination.clone());
        let position = args.position;
        let trip_name = args.trip;
        let decided = args.decided.unwrap_or(true);
        let trip = tokio::task::spawn_blocking(move || -> anyhow::Result<Trip> {
            let trip = find_trip_or_list(&store, user_id, &trip_name)?;
            let expected = ExpectedSegment {
                origin: &leg_origin,
                destination: &leg_destination,
                departure_date: leg_date.as_deref(),
            };
            store.add_candidate(trip.id, position, expected, candidate, decided).map_err(lost_trip_race)
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;

        let mut view = TripView::of(trip);
        if date_unchecked {
            // Reported rather than swallowed: an itinerary that looks
            // entirely correct is exactly the failure mode the date guard
            // exists to close, and skipping the check silently would be
            // that failure mode with extra steps.
            let segment_date = view
                .trip
                .segments
                .iter()
                .find(|s| s.position == position)
                .map(|s| s.departure_date.as_str())
                .unwrap_or("?");
            view.notes.push(format!(
                "the flight just added to segment {position} stated no departure time, so its \
                 date could not be checked against the segment's ({segment_date}) — confirm it \
                 is really on that date before relying on this"
            ));
        }
        Ok(view)
    }
}

#[derive(Debug, Deserialize)]
pub struct ChooseOptionArgs {
    pub trip: String,
    pub position: i64,
    pub candidate: i64,
}

pub struct ChooseTripOptionTool {
    pub store: Store,
    pub user_id: i64,
}

impl Tool for ChooseTripOptionTool {
    const NAME: &'static str = "choose_trip_option";
    type Error = StoreToolError;
    type Args = ChooseOptionArgs;
    type Output = TripView;

    fn description(&self) -> String {
        "Settle which of a segment's parked options the traveller is taking, \
         by its option number as shown by show_trip. Use this when they decide \
         later — by then the offer that produced the option has expired, so \
         there is no offer_id left to name it by."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "trip": {"type": "string", "description": "the trip's name"},
                "position": {"type": "integer", "description": "which segment, 1-based"},
                "candidate": {"type": "integer", "description": "which option on that segment, as numbered by show_trip"}
            },
            "required": ["trip", "position", "candidate"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let store = self.store.clone();
        let user_id = self.user_id;
        tokio::task::spawn_blocking(move || -> anyhow::Result<Trip> {
            let trip = find_trip_or_list(&store, user_id, &args.trip)?;
            store.choose_candidate(trip.id, args.position, args.candidate).map_err(lost_trip_race)
        })
        .await
        .map_err(internal)?
        .map(TripView::of)
        .map_err(internal)
    }
}

#[derive(Debug, Deserialize)]
pub struct ShowTripArgs {
    #[serde(default)]
    pub trip: Option<String>,
}

#[derive(Debug, PartialEq, serde::Serialize)]
pub struct TripList {
    pub trips: Vec<TripView>,
}

pub struct ShowTripTool {
    pub store: Store,
    pub user_id: i64,
}

impl Tool for ShowTripTool {
    const NAME: &'static str = "show_trip";
    type Error = StoreToolError;
    type Args = ShowTripArgs;
    type Output = TripList;

    fn description(&self) -> String {
        "Show one trip in full, or every trip the traveller has if no name is \
         given. Each segment's options are numbered; those numbers are what \
         choose_trip_option takes. Call this before editing a trip so the \
         positions and option numbers you use are the current ones."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "trip": {"type": "string", "description": "the trip's name; omit to list them all"}
            }
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let store = self.store.clone();
        let user_id = self.user_id;
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Trip>> {
            match &args.trip {
                None => store.list_trips(user_id),
                Some(name) => find_trip_or_list(&store, user_id, name).map(|trip| vec![trip]),
            }
        })
        .await
        .map_err(internal)?
        .map(|trips| TripList { trips: trips.into_iter().map(TripView::of).collect() })
        .map_err(internal)
    }
}

#[derive(Debug, Deserialize)]
pub struct DropSegmentArgs {
    pub trip: String,
    pub position: i64,
    /// Without this, the whole segment goes.
    #[serde(default)]
    pub candidate: Option<i64>,
}

pub struct DropTripSegmentTool {
    pub store: Store,
    pub user_id: i64,
}

impl Tool for DropTripSegmentTool {
    const NAME: &'static str = "drop_trip_segment";
    type Error = StoreToolError;
    type Args = DropSegmentArgs;
    type Output = TripView;

    fn description(&self) -> String {
        "Remove a segment from a trip, or with candidate given, just one of \
         that segment's parked options. Later segments are renumbered, so call \
         show_trip afterwards before using positions again."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "trip": {"type": "string", "description": "the trip's name"},
                "position": {"type": "integer", "description": "which segment, 1-based"},
                "candidate": {"type": "integer", "description": "drop only this option, leaving the segment"}
            },
            "required": ["trip", "position"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let store = self.store.clone();
        let user_id = self.user_id;
        tokio::task::spawn_blocking(move || -> anyhow::Result<Trip> {
            let trip = find_trip_or_list(&store, user_id, &args.trip)?;
            match args.candidate {
                Some(candidate) => store.drop_candidate(trip.id, args.position, candidate),
                None => store.drop_segment(trip.id, args.position),
            }
        })
        .await
        .map_err(internal)?
        .map(TripView::of)
        .map_err(internal)
    }
}

#[derive(Debug, Deserialize)]
pub struct DeleteTripArgs {
    pub trip: String,
}

#[derive(Debug, PartialEq, serde::Serialize)]
pub struct DeleteTripResult {
    pub deleted: bool,
    pub trip: String,
    /// The traveller's real trip names, filled in only when nothing was
    /// deleted. Not an error - deleting something already gone is the
    /// state the caller wanted - but a mistyped name still needs
    /// somewhere to go. Same convention as `TripView::notes`: empty when
    /// there is nothing to add.
    pub notes: Vec<String>,
}

pub struct DeleteTripTool {
    pub store: Store,
    pub user_id: i64,
}

impl Tool for DeleteTripTool {
    const NAME: &'static str = "delete_trip";
    type Error = StoreToolError;
    type Args = DeleteTripArgs;
    type Output = DeleteTripResult;

    fn description(&self) -> String {
        "Delete a whole trip and everything on it. Use when the traveller \
         abandons a plan, or to clear one created by a mistyped name."
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
        tokio::task::spawn_blocking(move || -> anyhow::Result<DeleteTripResult> {
            let deleted = store.delete_trip(user_id, &args.trip)?;
            let notes = match deleted {
                true => Vec::new(),
                // A mistyped name is indistinguishable here from one
                // already deleted, so both get the same real names to
                // correct against rather than a silent no-op.
                false => vec![format!(
                    "nothing called {:?} to delete. This traveller has: {}",
                    args.trip,
                    trip_names(&store, user_id)?
                )],
            };
            Ok(DeleteTripResult { deleted, trip: name, notes })
        })
        .await
        .map_err(internal)?
        .map_err(internal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Store, TripCandidate, TripSegment};
    use crate::tools::duffel::{Flight, Leg, PriceStatus, Source};
    use crate::tools::shown::ShownFlights;
    use rig::tool::Tool;
    use std::sync::Arc;
    use std::time::Instant;
    use tempfile::TempDir;

    fn setup() -> (Store, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("t.duckdb")).unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn adding_a_segment_creates_the_trip_and_returns_all_of_it() {
        // Returning the whole trip is the mitigation for several named trips:
        // editing "Setpember" comes back as an unfamiliar one-segment trip, in
        // the reply, where the traveller sees it.
        let (store, _d) = setup();
        let tool = AddTripSegmentTool { store: store.clone(), user_id: 7 };
        let trip = tool
            .call(AddSegmentArgs {
                trip: "September".into(),
                origin: "ams".into(),
                destination: "lis".into(),
                departure_date: "2026-09-03".into(),
                position: None,
                adults: Some(2),
                cabin_class: None,
            })
            .await
            .unwrap();
        assert_eq!(trip.trip.name, "September");
        assert_eq!(trip.trip.adults, 2);
        assert_eq!(trip.trip.segments.len(), 1);
        assert_eq!(trip.trip.segments[0].origin, "AMS", "codes are normalised on the way in");
        assert_eq!(trip.trip.segments[0].destination, "LIS");
    }

    #[tokio::test]
    async fn a_bad_airport_code_is_refused_before_anything_is_written() {
        let (store, _d) = setup();
        let tool = AddTripSegmentTool { store: store.clone(), user_id: 7 };
        let err = tool
            .call(AddSegmentArgs {
                trip: "September".into(),
                origin: "Amsterdam".into(),
                destination: "LIS".into(),
                departure_date: "2026-09-03".into(),
                position: None,
                adults: None,
                cabin_class: None,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("IATA"), "got: {err}");
        assert!(store.find_trip(7, "September").unwrap().is_none(), "nothing was created");
    }

    #[tokio::test]
    async fn an_unpadded_date_is_stored_zero_padded() {
        // dates_run_forwards compares departure_date as text, which only
        // agrees with date order when every date is zero-padded — that
        // invariant is established here, at the tool boundary, or nowhere.
        let (store, _d) = setup();
        let tool = AddTripSegmentTool { store: store.clone(), user_id: 7 };
        let trip = tool
            .call(AddSegmentArgs {
                trip: "September".into(),
                origin: "AMS".into(),
                destination: "LIS".into(),
                departure_date: "2026-9-3".into(),
                position: None,
                adults: None,
                cabin_class: None,
            })
            .await
            .unwrap();
        assert_eq!(
            trip.trip.segments[0].departure_date, "2026-09-03",
            "unpadded input must be normalised, not stored as typed"
        );
    }

    #[test]
    fn a_trip_deleted_between_creating_and_writing_the_segment_explains_itself() {
        // upsert_trip and add_segment are two separate lock acquisitions, so
        // a concurrent delete_trip landing between them makes add_segment's
        // plain "no such trip" surface — on a tool documented to create the
        // trip when the name is new, which reads as nonsense here.
        let wrapped = lost_trip_race(anyhow::anyhow!("no such trip"));
        assert!(wrapped.to_string().contains("try again"), "got: {wrapped}");

        // Only that one message is relabelled; anything else passes through.
        let other = lost_trip_race(anyhow::anyhow!("origin and destination are both AMS"));
        assert_eq!(other.to_string(), "origin and destination are both AMS");
    }

    fn segment(position: i64, origin: &str, destination: &str, date: &str) -> TripSegment {
        TripSegment {
            position,
            origin: origin.to_string(),
            destination: destination.to_string(),
            departure_date: date.to_string(),
            candidates: Vec::new(),
        }
    }

    #[test]
    fn a_gap_between_segments_is_reported_not_refused() {
        // Landing at FCO and leaving from FLR is a real trip with a train
        // in the middle. Saying nothing would let it read as continuous.
        let segments = vec![
            segment(1, "AMS", "FCO", "2026-09-03"),
            segment(2, "FLR", "AMS", "2026-09-10"),
        ];
        let notes = itinerary_notes(&segments);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("FCO"), "got: {}", notes[0]);
        assert!(notes[0].contains("FLR"), "got: {}", notes[0]);
    }

    #[test]
    fn a_continuous_itinerary_has_nothing_to_say() {
        let segments = vec![
            segment(1, "AMS", "LIS", "2026-09-03"),
            segment(2, "LIS", "AMS", "2026-09-10"),
        ];
        assert!(itinerary_notes(&segments).is_empty());
    }

    fn chosen(numbers: &str, arriving: &str, departing: &str) -> TripCandidate {
        TripCandidate {
            candidate: 1,
            chosen: true,
            airline: "KLM".to_string(),
            flight_numbers: numbers.to_string(),
            itinerary: "somewhere".to_string(),
            departing_at_local: Some(departing.to_string()),
            arriving_at_local: Some(arriving.to_string()),
            duration_minutes: None,
            quoted_price: None,
            quoted_currency: None,
            source: None,
        }
    }

    #[test]
    fn a_tight_turnaround_at_the_same_airport_is_flagged() {
        // Both times are local to the airport they share, so unlike the two
        // ends of a leg these *are* comparable — which is the only reason
        // this check is allowed to subtract them at all.
        let mut segments = vec![
            segment(1, "AMS", "LIS", "2026-09-03"),
            segment(2, "LIS", "FCO", "2026-09-03"),
        ];
        segments[0].candidates = vec![chosen("TP675", "2026-09-03T12:15:00", "2026-09-03T10:05:00")];
        segments[1].candidates = vec![chosen("TP830", "2026-09-03T16:00:00", "2026-09-03T13:30:00")];
        let notes = itinerary_notes(&segments);
        assert_eq!(notes.len(), 1, "1h15m between landing and the next departure");
        assert!(notes[0].contains("1h 15m"), "got: {}", notes[0]);

        // Comfortable, so nothing to say.
        segments[1].candidates = vec![chosen("TP830", "2026-09-03T22:00:00", "2026-09-03T19:30:00")];
        assert!(itinerary_notes(&segments).is_empty());
    }

    #[test]
    fn a_turnaround_that_leaves_before_it_lands_is_impossible_not_tight() {
        // Same airport, so these times really are comparable, and the next
        // flight leaves 2.5 hours before the previous one lands. That is a
        // different problem from a tight connection and must not be
        // silently swallowed by a range check that only looks at positive
        // minutes.
        let mut segments = vec![
            segment(1, "AMS", "LIS", "2026-09-03"),
            segment(2, "LIS", "FCO", "2026-09-03"),
        ];
        segments[0].candidates = vec![chosen("TP675", "2026-09-03T14:00:00", "2026-09-03T10:05:00")];
        segments[1].candidates = vec![chosen("TP830", "2026-09-03T16:00:00", "2026-09-03T11:30:00")];
        let notes = itinerary_notes(&segments);
        assert_eq!(notes.len(), 1, "got: {notes:?}");
        assert!(notes[0].contains("cannot be flown"), "got: {}", notes[0]);
        assert!(
            !notes[0].contains("only"),
            "an impossible connection must not read as a merely tight one: {}",
            notes[0]
        );
    }

    #[test]
    fn a_turnaround_is_not_computed_across_a_gap() {
        // Different airports means two clocks in two places, and the
        // codebase's rule is that those are never subtracted. The gap note
        // covers it instead.
        let mut segments = vec![
            segment(1, "AMS", "FCO", "2026-09-03"),
            segment(2, "FLR", "AMS", "2026-09-03"),
        ];
        segments[0].candidates = vec![chosen("KL1", "2026-09-03T12:15:00", "2026-09-03T10:05:00")];
        segments[1].candidates = vec![chosen("KL2", "2026-09-03T16:00:00", "2026-09-03T13:00:00")];
        let notes = itinerary_notes(&segments);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("FCO"), "only the gap is reported: {}", notes[0]);
    }

    #[test]
    fn dates_running_backwards_are_refused_rather_than_sorted() {
        // Sorting them would change the trip the traveller described, and a
        // multi-slice request built from them is meaningless either way.
        let segments = vec![
            segment(1, "AMS", "LIS", "2026-09-07"),
            segment(2, "LIS", "FCO", "2026-09-03"),
        ];
        let problem = dates_run_forwards(&segments).unwrap_err();
        assert!(problem.contains("2026-09-03"), "got: {problem}");

        let ok = vec![
            segment(1, "AMS", "LIS", "2026-09-03"),
            segment(2, "LIS", "FCO", "2026-09-07"),
        ];
        assert!(dates_run_forwards(&ok).is_ok());
        // Two segments on one day is a legitimate same-day connection.
        let same_day = vec![
            segment(1, "AMS", "LIS", "2026-09-03"),
            segment(2, "LIS", "FCO", "2026-09-03"),
        ];
        assert!(dates_run_forwards(&same_day).is_ok());
    }

    fn one_way(id: &str, origin: &str, destination: &str, date: &str, numbers: &[&str]) -> Flight {
        Flight {
            offer_id: id.to_string(),
            source: Source::Duffel,
            price_status: PriceStatus::Bookable,
            self_transfer: false,
            airline: "KLM".to_string(),
            price: 940.0,
            currency: "EUR".to_string(),
            legs: vec![Leg {
                origin: origin.to_string(),
                destination: destination.to_string(),
                departing_at_local: format!("{date}T10:05:00"),
                arriving_at_local: format!("{date}T12:15:00"),
                duration_minutes: Some(130),
                duration: Some("2h 10m".to_string()),
                stops: 0,
                flights: numbers.iter().map(|s| s.to_string()).collect(),
                connections: Vec::new(),
                itinerary: format!("{origin} 10:05 ✈ {destination} 12:15"),
            }],
            total_minutes: Some(130),
            total_duration: Some("2h 10m".to_string()),
            checked_bags: Some(1),
            carry_on_bags: Some(1),
            expires_at: None,
        }
    }

    #[tokio::test]
    async fn two_options_can_sit_on_one_segment_until_a_choice_is_made() {
        let (store, _d) = setup();
        let shown = Arc::new(ShownFlights::default());
        shown.remember(
            99,
            vec![
                one_way("nonstop", "AMS", "NRT", "2026-09-03", &["KL861"]),
                one_way("via-hkg", "AMS", "NRT", "2026-09-03", &["CX270", "CX500"]),
            ],
            Instant::now(),
        );
        let add_seg = AddTripSegmentTool { store: store.clone(), user_id: 7 };
        add_seg
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

        let add_opt =
            AddTripOptionTool { store: store.clone(), user_id: 7, shown: shown.clone(), chat_id: 99 };
        for id in ["nonstop", "via-hkg"] {
            add_opt
                .call(AddOptionArgs {
                    trip: "Japan".into(),
                    position: 1,
                    offer_id: id.into(),
                    decided: Some(false),
                })
                .await
                .unwrap();
        }
        let view = add_opt
            .call(AddOptionArgs {
                trip: "Japan".into(),
                position: 1,
                offer_id: "nonstop".into(),
                decided: Some(false),
            })
            .await
            .unwrap();
        assert_eq!(view.trip.segments[0].candidates.len(), 3, "the same flight may be parked twice");
        assert_eq!(view.trip.segments[0].candidates[1].flight_numbers, "CX270,CX500");
        assert!(view.trip.segments[0].candidates.iter().all(|c| !c.chosen));

        let choose = ChooseTripOptionTool { store: store.clone(), user_id: 7 };
        let view = choose
            .call(ChooseOptionArgs { trip: "Japan".into(), position: 1, candidate: 2 })
            .await
            .unwrap();
        assert!(view.trip.segments[0].candidates[1].chosen);
    }

    #[tokio::test]
    async fn a_flight_this_chat_was_not_shown_is_refused_without_touching_the_trip() {
        let (store, _d) = setup();
        let shown = Arc::new(ShownFlights::default());
        shown.remember(99, vec![one_way("real", "AMS", "NRT", "2026-09-03", &["KL861"])], Instant::now());
        let add_seg = AddTripSegmentTool { store: store.clone(), user_id: 7 };
        add_seg
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

        // Another chat's tool, holding the same store.
        let elsewhere =
            AddTripOptionTool { store: store.clone(), user_id: 7, shown: shown.clone(), chat_id: 12 };
        let err = elsewhere
            .call(AddOptionArgs {
                trip: "Japan".into(),
                position: 1,
                offer_id: "real".into(),
                decided: None,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("was not shown"), "got: {err}");
        assert!(store.find_trip(7, "Japan").unwrap().unwrap().segments[0].candidates.is_empty());
    }

    #[tokio::test]
    async fn a_return_offer_cannot_be_bound_to_a_single_segment() {
        // A segment is one direction. A two-leg offer bound to it would price
        // the return twice and quietly misdescribe the trip.
        let (store, _d) = setup();
        let shown = Arc::new(ShownFlights::default());
        let mut round_trip = one_way("both-ways", "AMS", "NRT", "2026-09-03", &["KL861"]);
        round_trip.legs.push(one_way("x", "NRT", "AMS", "2026-09-10", &["KL862"]).legs.remove(0));
        shown.remember(99, vec![round_trip], Instant::now());

        let add_seg = AddTripSegmentTool { store: store.clone(), user_id: 7 };
        add_seg
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

        let add_opt = AddTripOptionTool { store, user_id: 7, shown, chat_id: 99 };
        let err = add_opt
            .call(AddOptionArgs {
                trip: "Japan".into(),
                position: 1,
                offer_id: "both-ways".into(),
                decided: None,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("return"), "got: {err}");
    }

    #[tokio::test]
    async fn an_option_for_a_different_route_is_refused() {
        let (store, _d) = setup();
        let shown = Arc::new(ShownFlights::default());
        shown.remember(99, vec![one_way("wrong", "AMS", "LIS", "2026-09-03", &["TP675"])], Instant::now());
        let add_seg = AddTripSegmentTool { store: store.clone(), user_id: 7 };
        add_seg
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

        let add_opt = AddTripOptionTool { store, user_id: 7, shown, chat_id: 99 };
        let err = add_opt
            .call(AddOptionArgs {
                trip: "Japan".into(),
                position: 1,
                offer_id: "wrong".into(),
                decided: None,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("AMS→NRT"), "got: {err}");
    }

    #[tokio::test]
    async fn an_option_for_the_right_route_but_the_wrong_date_is_refused() {
        // Finalisation re-searches a segment on its own departure_date and
        // matches the stored flight numbers against that day's results. A
        // date-mismatched bind either reports a flight as no longer sold,
        // or — because airlines reuse flight numbers day to day — silently
        // matches a different flight that happens to share the number.
        let (store, _d) = setup();
        let shown = Arc::new(ShownFlights::default());
        shown.remember(99, vec![one_way("wrong-date", "AMS", "NRT", "2026-09-03", &["KL861"])], Instant::now());
        let add_seg = AddTripSegmentTool { store: store.clone(), user_id: 7 };
        add_seg
            .call(AddSegmentArgs {
                trip: "Japan".into(),
                origin: "AMS".into(),
                destination: "NRT".into(),
                departure_date: "2026-09-05".into(),
                position: None,
                adults: None,
                cabin_class: None,
            })
            .await
            .unwrap();

        let add_opt = AddTripOptionTool { store, user_id: 7, shown, chat_id: 99 };
        let err = add_opt
            .call(AddOptionArgs {
                trip: "Japan".into(),
                position: 1,
                offer_id: "wrong-date".into(),
                decided: None,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("2026-09-03"), "got: {err}");
        assert!(err.to_string().contains("2026-09-05"), "got: {err}");
    }

    #[tokio::test]
    async fn a_flight_with_no_stated_departure_time_is_added_with_a_note_not_a_refusal() {
        // Duffel's own parser falls back to an empty string when an offer's
        // leg states no departure time (see `duffel::leg`). That is
        // "nothing to check", not "checked and failed" — refusing it would
        // be wrong. But saying nothing would leave the date silently
        // unverified, which is the exact failure the guard exists to close.
        let (store, _d) = setup();
        let shown = Arc::new(ShownFlights::default());
        let mut flight = one_way("no-time", "AMS", "NRT", "2026-09-03", &["KL861"]);
        flight.legs[0].departing_at_local = String::new();
        shown.remember(99, vec![flight], Instant::now());
        let add_seg = AddTripSegmentTool { store: store.clone(), user_id: 7 };
        add_seg
            .call(AddSegmentArgs {
                trip: "Japan".into(),
                origin: "AMS".into(),
                destination: "NRT".into(),
                departure_date: "2026-09-05".into(),
                position: None,
                adults: None,
                cabin_class: None,
            })
            .await
            .unwrap();

        let add_opt = AddTripOptionTool { store, user_id: 7, shown, chat_id: 99 };
        let view = add_opt
            .call(AddOptionArgs {
                trip: "Japan".into(),
                position: 1,
                offer_id: "no-time".into(),
                decided: None,
            })
            .await
            .unwrap();
        assert_eq!(
            view.trip.segments[0].candidates.len(),
            1,
            "must not be refused for a date check that cannot run"
        );
        assert!(
            view.notes.iter().any(|n| n.contains("date could not be checked")),
            "the gap must be visible rather than silent: {:?}",
            view.notes
        );
    }

    #[tokio::test]
    async fn show_lists_every_trip_when_no_name_is_given() {
        let (store, _d) = setup();
        let add = AddTripSegmentTool { store: store.clone(), user_id: 7 };
        for name in ["Japan via HK", "Japan direct"] {
            add.call(AddSegmentArgs {
                trip: name.into(),
                origin: "AMS".into(),
                destination: "NRT".into(),
                departure_date: "2026-09-03".into(),
                position: None,
                adults: None,
                cabin_class: None,
            })
            .await
            .unwrap();
        }

        let show = ShowTripTool { store: store.clone(), user_id: 7 };
        let all = show.call(ShowTripArgs { trip: None }).await.unwrap();
        assert_eq!(all.trips.len(), 2);

        let one = show.call(ShowTripArgs { trip: Some("japan direct".into()) }).await.unwrap();
        assert_eq!(one.trips.len(), 1);
        assert_eq!(one.trips[0].trip.name, "Japan direct");

        let missing = show.call(ShowTripArgs { trip: Some("Peru".into()) }).await.unwrap_err();
        assert!(missing.to_string().contains("Japan direct"), "unknown names list the real ones: {missing}");
    }

    #[tokio::test]
    async fn dropping_takes_an_option_or_the_whole_segment() {
        let (store, _d) = setup();
        let shown = Arc::new(ShownFlights::default());
        shown.remember(99, vec![one_way("a", "AMS", "NRT", "2026-09-03", &["KL861"])], Instant::now());
        let add = AddTripSegmentTool { store: store.clone(), user_id: 7 };
        add.call(AddSegmentArgs {
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

        let drop = DropTripSegmentTool { store: store.clone(), user_id: 7 };
        let view = drop
            .call(DropSegmentArgs { trip: "Japan".into(), position: 1, candidate: Some(1) })
            .await
            .unwrap();
        assert_eq!(view.trip.segments.len(), 1, "the segment survives losing its option");
        assert!(view.trip.segments[0].candidates.is_empty());

        let view = drop
            .call(DropSegmentArgs { trip: "Japan".into(), position: 1, candidate: None })
            .await
            .unwrap();
        assert!(view.trip.segments.is_empty());
    }

    #[tokio::test]
    async fn deleting_a_trip_reports_when_there_was_nothing_to_delete() {
        let (store, _d) = setup();
        AddTripSegmentTool { store: store.clone(), user_id: 7 }
            .call(AddSegmentArgs {
                trip: "Setpember".into(),
                origin: "AMS".into(),
                destination: "LIS".into(),
                departure_date: "2026-09-03".into(),
                position: None,
                adults: None,
                cabin_class: None,
            })
            .await
            .unwrap();
        let delete = DeleteTripTool { store: store.clone(), user_id: 7 };
        assert!(delete.call(DeleteTripArgs { trip: "Setpember".into() }).await.unwrap().deleted);
        assert!(!delete.call(DeleteTripArgs { trip: "Setpember".into() }).await.unwrap().deleted);
    }

    // The four tests below are the ones that would have caught the spec
    // clause that shipped unimplemented: every tool that refuses an
    // unknown trip name must say which ones actually exist, not just that
    // the given one doesn't. Only `show_trip`'s missing-name path had a
    // test before this; the other three bailed with a bare "no trip
    // called X" and nothing checked for the list.

    #[tokio::test]
    async fn drop_trip_segment_on_an_unknown_name_lists_the_real_ones() {
        let (store, _d) = setup();
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

        let drop = DropTripSegmentTool { store, user_id: 7 };
        let err = drop
            .call(DropSegmentArgs { trip: "Japen".into(), position: 1, candidate: None })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Japan"), "unknown names list the real ones: {err}");
    }

    #[tokio::test]
    async fn choose_trip_option_on_an_unknown_name_lists_the_real_ones() {
        let (store, _d) = setup();
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

        let choose = ChooseTripOptionTool { store, user_id: 7 };
        let err = choose
            .call(ChooseOptionArgs { trip: "Japen".into(), position: 1, candidate: 1 })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Japan"), "unknown names list the real ones: {err}");
    }

    #[tokio::test]
    async fn add_trip_option_on_an_unknown_name_lists_the_real_ones() {
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

        // The offer must be a real, shown one so the two guards ahead of
        // the trip lookup both pass and it is that lookup being tested.
        let add_opt = AddTripOptionTool { store, user_id: 7, shown, chat_id: 99 };
        let err = add_opt
            .call(AddOptionArgs { trip: "Japen".into(), position: 1, offer_id: "a".into(), decided: None })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Japan"), "unknown names list the real ones: {err}");
    }

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
    fn a_return_offer_is_never_mistaken_for_a_single_leg_connection() {
        // add_trip_option refuses to bind a return offer to a segment, so a
        // stored candidate can only ever have come from a single leg.
        // Flattening every leg before joining — the old behaviour — makes a
        // two-leg return with one flight number each join to the same
        // string as a genuine one-leg, two-flight connection.
        let mut round_trip = one_way("both-ways", "AMS", "NRT", "2026-09-03", &["KL861"]);
        round_trip.legs.push(one_way("x", "NRT", "AMS", "2026-09-10", &["KL862"]).legs.remove(0));
        let offers = vec![round_trip];
        assert!(match_candidate(&parked("KL861,KL862", 780.0), &offers).is_none());
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

    // The currency a segment's total is labelled with must come from the
    // same place as the number itself: `price_now`, today's search result.
    // `quoted_currency` is only what this cost when it was parked, and the
    // whole point of finalisation is that figure is stale, so this helper
    // keeps the two independent — a caller that wants them to agree passes
    // the same string twice; a caller testing the laundering bug does not.
    fn priced_now(quoted_currency: &str, price_now: Option<f64>, price_now_currency: Option<&str>) -> PricedSegment {
        PricedSegment {
            position: 1,
            route: "AMS→LIS".to_string(),
            departure_date: "2026-09-03".to_string(),
            chosen: PricedOption {
                airline: "TAP".to_string(),
                flight_numbers: "TP675".to_string(),
                itinerary: "AMS 10:05 ✈ LIS 12:15".to_string(),
                quoted_price: Some(118.0),
                quoted_currency: Some(quoted_currency.to_string()),
                price_now,
                price_now_currency: price_now_currency.map(str::to_string),
                moved: None,
                still_offered: price_now.is_some(),
            },
            also_considered: Vec::new(),
            booking: Vec::new(),
        }
    }

    /// A segment priced now in the same currency it was parked in — the
    /// ordinary case, and what most tests here want.
    fn priced(price: Option<f64>, currency: &str) -> PricedSegment {
        priced_now(currency, price, price.map(|_| currency))
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
    fn a_price_with_no_stated_current_currency_refuses_rather_than_being_summed_under_the_parked_one() {
        // The bug this guards: both were parked in EUR, so the old check —
        // which looked at quoted_currency — saw agreement and summed anyway,
        // labelling the total EUR even though this segment's live price
        // carries no currency of its own. Nothing in "600.00 EUR" would say
        // that half of it was never actually priced in EUR.
        let unlabelled = priced_now("EUR", Some(500.0), None);
        let segments = vec![priced(Some(100.0), "EUR"), unlabelled];
        assert!(separate_total(&segments).is_err(), "a price with no currency must not be summed");
    }

    #[test]
    fn segments_whose_current_currencies_disagree_refuse_even_when_the_parked_ones_matched() {
        // Both were parked in EUR, but the market has moved and one is now
        // only sold in USD. A total has to be labelled with what was
        // actually summed, not with a currency from before either price on
        // it went live.
        let now_usd = priced_now("EUR", Some(200.0), Some("USD"));
        let segments = vec![priced(Some(100.0), "EUR"), now_usd];
        let problem = separate_total(&segments).unwrap_err();
        assert!(problem.contains("EUR"), "got: {problem}");
        assert!(problem.contains("USD"), "got: {problem}");
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

    #[test]
    fn every_combination_of_the_two_totals_reads_unambiguously() {
        // Both present: a stated comparison.
        let both = comparison_notes(2, Some(100.0), Some(142.0), "EUR").join(" ");
        assert!(both.contains("more") || both.contains("cheaper"), "got: {both}");

        // Separate present, one-ticket missing: says the comparison could
        // not be made — the case the original tests already covered.
        let only_separate = comparison_notes(2, Some(100.0), None, "EUR").join(" ");
        assert!(only_separate.contains("could not"), "got: {only_separate}");
        assert!(
            !only_separate.contains("more") && !only_separate.contains("cheaper"),
            "a missing comparison must not read as a verdict: {only_separate}"
        );

        // One-ticket present, separate missing: the case this function used
        // to say nothing about at all, letting a real one-ticket price sit
        // there with no explanation for why there was nothing to compare it
        // against.
        let only_one_ticket = comparison_notes(2, None, Some(142.0), "EUR").join(" ");
        assert!(only_one_ticket.contains("could not"), "got: {only_one_ticket}");
        assert!(
            !only_one_ticket.contains("more") && !only_one_ticket.contains("cheaper"),
            "a missing comparison must not read as a verdict either way: {only_one_ticket}"
        );

        // Neither present: nothing to compare, so nothing to say here — the
        // reasons each total is missing are reported by their own callers.
        let neither = comparison_notes(2, None, None, "EUR");
        assert!(neither.is_empty(), "got: {neither:?}");
    }

    #[test]
    fn an_unknown_currency_does_not_leave_a_double_space_in_the_comparison() {
        // The caller passes "" when no currency could be determined at all.
        // Dropping the placeholder cleanly matters more here than it looks:
        // "one ticket costs 42.00  more" reads like a copy-paste error, which
        // undermines trust in the number right next to it.
        let notes = comparison_notes(2, Some(100.0), Some(142.0), "");
        let joined = notes.join(" ");
        assert!(!joined.contains("  "), "no double space when the currency is unknown: {joined}");
        assert!(joined.contains("42.00"), "got: {joined}");
    }

    #[test]
    fn a_trip_that_cannot_be_priced_is_refused_before_anything_is_bought() {
        use crate::store::TripSegment as Seg;

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

    #[tokio::test]
    async fn deleting_an_unknown_trip_lists_the_real_ones_in_notes() {
        // Not an error - deleting something already gone is the state the
        // caller wanted - but a mistyped name still needs somewhere to go.
        let (store, _d) = setup();
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

        let delete = DeleteTripTool { store, user_id: 7 };
        let result = delete.call(DeleteTripArgs { trip: "Japen".into() }).await.unwrap();
        assert!(!result.deleted);
        assert!(
            result.notes.iter().any(|n| n.contains("Japan")),
            "unknown names list the real ones: {:?}",
            result.notes
        );
    }

    // finalise_trip: a re-pricing pass, never a checkout. Every segment is
    // searched again for today's price, and the same offer_request response
    // stands in for both providers here since the tool is only asked to
    // search Duffel in these tests (ignav: None).

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
        assert_eq!(chosen.price_now_currency.as_deref(), Some("EUR"), "the currency the fresh price is actually in");
        assert_eq!(chosen.moved, Some(40.0));
        assert!(chosen.still_offered);
        // One segment, so there is no single-ticket comparison to make, and
        // the output has to say so rather than let the separate total stand
        // alone.
        assert!(out.one_ticket_total.is_none());
        assert!(out.notes.iter().any(|n| n.contains("could not")), "notes: {:?}", out.notes);

        // Priced means priced: the trip records it, and the parked price is
        // left exactly as it was.
        let trip = store.find_trip(7, "Japan").unwrap().unwrap();
        assert_eq!(trip.status, "finalised");
        assert_eq!(
            trip.segments[0].candidates[0].quoted_price,
            Some(940.0),
            "quoted_price is never overwritten by a re-price"
        );
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

    #[tokio::test]
    async fn an_undecided_trip_is_refused_before_any_search_is_bought() {
        // Everything that can refuse, refuses before a single paid search: a
        // trip that cannot be priced must cost nothing to discover. The
        // server below has no mock mounted, so any request to it fails —
        // proving the tool never got that far.
        let server = MockServer::start().await;
        let (store, _d) = setup();
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

        let duffel = crate::tools::duffel::DuffelClient::new(
            reqwest::Client::new(),
            "test".to_string(),
            server.uri(),
        );
        let err = FinaliseTripTool {
            store,
            user_id: 7,
            duffel: Some(duffel),
            ignav: None,
            budget: Arc::new(crate::tools::budget::FlightBudget::default()),
        }
        .call(FinaliseArgs { trip: "Japan".into() })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no flight"), "got: {err}");
    }

    #[tokio::test]
    async fn a_missing_single_ticket_price_never_reads_as_a_verdict_for_separate_booking() {
        // Two segments, so a single-ticket comparison is attempted, but the
        // multi-city endpoint returns nothing usable here — no offer at all.
        // The output must say the comparison is missing, not let the
        // separate total stand alone as if it had won.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/air/offer_requests"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": {"offers": []}})))
            .mount(&server)
            .await;

        let (store, _d) = setup();
        let shown = Arc::new(ShownFlights::default());
        shown.remember(
            99,
            vec![
                one_way("a", "AMS", "LIS", "2026-09-03", &["TP675"]),
                one_way("b", "LIS", "AMS", "2026-09-07", &["TP676"]),
            ],
            Instant::now(),
        );
        let add_seg = AddTripSegmentTool { store: store.clone(), user_id: 7 };
        add_seg
            .call(AddSegmentArgs {
                trip: "Lisbon".into(),
                origin: "AMS".into(),
                destination: "LIS".into(),
                departure_date: "2026-09-03".into(),
                position: None,
                adults: None,
                cabin_class: None,
            })
            .await
            .unwrap();
        add_seg
            .call(AddSegmentArgs {
                trip: "Lisbon".into(),
                origin: "LIS".into(),
                destination: "AMS".into(),
                departure_date: "2026-09-07".into(),
                position: None,
                adults: None,
                cabin_class: None,
            })
            .await
            .unwrap();
        let add_opt = AddTripOptionTool { store: store.clone(), user_id: 7, shown, chat_id: 99 };
        add_opt
            .call(AddOptionArgs { trip: "Lisbon".into(), position: 1, offer_id: "a".into(), decided: None })
            .await
            .unwrap();
        add_opt
            .call(AddOptionArgs { trip: "Lisbon".into(), position: 2, offer_id: "b".into(), decided: None })
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
        .call(FinaliseArgs { trip: "Lisbon".into() })
        .await
        .unwrap();

        assert!(out.one_ticket_total.is_none(), "no offer came back, so there is no single-ticket total");
        assert!(
            out.notes.iter().any(|n| n.contains("could not")),
            "a missing comparison must say so rather than reading as a verdict: {:?}",
            out.notes
        );
        assert!(
            !out.notes.iter().any(|n| n.to_lowercase().contains("better")),
            "no note may claim separate booking won by default: {:?}",
            out.notes
        );
    }

    #[tokio::test]
    async fn finalising_never_overwrites_what_a_candidate_was_quoted_at() {
        // quoted_price and quoted_currency mean "what this cost when it was
        // chosen"; refreshing them on every finalise would make price_move
        // shrink to nothing every time anyone looked, which is the one
        // number this feature exists to produce.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/air/offer_requests"))
            .respond_with(ResponseTemplate::new(200).set_body_json(duffel_offer("1200.00", &["KL861"])))
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
        let tool = FinaliseTripTool {
            store: store.clone(),
            user_id: 7,
            duffel: Some(duffel),
            ignav: None,
            budget: Arc::new(crate::tools::budget::FlightBudget::default()),
        };
        tool.call(FinaliseArgs { trip: "Japan".into() }).await.unwrap();
        tool.call(FinaliseArgs { trip: "Japan".into() }).await.unwrap();

        let trip = store.find_trip(7, "Japan").unwrap().unwrap();
        assert_eq!(
            trip.segments[0].candidates[0].quoted_price,
            Some(940.0),
            "two finalisations in a row must not move the parked price at all"
        );
        assert_eq!(trip.segments[0].candidates[0].quoted_currency.as_deref(), Some("EUR"));
    }

    #[tokio::test]
    async fn a_runner_up_option_is_priced_from_the_same_search_as_the_chosen_one() {
        // Runners-up come free with the same search: a decision made a week
        // ago deserves checking against what the alternatives cost today.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/air/offer_requests"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": {"offers": [
                {
                    "id": "off_1", "total_amount": "980.00", "total_currency": "EUR",
                    "owner": {"name": "KLM"},
                    "slices": [{
                        "origin": {"iata_code": "AMS"}, "destination": {"iata_code": "NRT"},
                        "duration": "PT2H10M",
                        "segments": [{
                            "marketing_carrier_flight_number": "861",
                            "marketing_carrier": {"iata_code": "KL", "name": "KLM"},
                            "origin": {"iata_code": "AMS"}, "destination": {"iata_code": "NRT"},
                            "departing_at": "2026-09-03T10:05:00", "arriving_at": "2026-09-03T12:15:00",
                            "duration": "PT2H10M"
                        }]
                    }]
                },
                {
                    "id": "off_2", "total_amount": "820.00", "total_currency": "EUR",
                    "owner": {"name": "Cathay Pacific"},
                    "slices": [{
                        "origin": {"iata_code": "AMS"}, "destination": {"iata_code": "NRT"},
                        "duration": "PT4H00M",
                        "segments": [
                            {
                                "marketing_carrier_flight_number": "270",
                                "marketing_carrier": {"iata_code": "CX", "name": "Cathay Pacific"},
                                "origin": {"iata_code": "AMS"}, "destination": {"iata_code": "HKG"},
                                "departing_at": "2026-09-03T10:05:00", "arriving_at": "2026-09-03T12:15:00",
                                "duration": "PT2H10M"
                            },
                            {
                                "marketing_carrier_flight_number": "500",
                                "marketing_carrier": {"iata_code": "CX", "name": "Cathay Pacific"},
                                "origin": {"iata_code": "HKG"}, "destination": {"iata_code": "NRT"},
                                "departing_at": "2026-09-03T14:05:00", "arriving_at": "2026-09-03T16:15:00",
                                "duration": "PT1H50M"
                            }
                        ]
                    }]
                }
            ]}})))
            .mount(&server)
            .await;

        let (store, _d) = setup();
        let shown = Arc::new(ShownFlights::default());
        shown.remember(
            99,
            vec![
                one_way("nonstop", "AMS", "NRT", "2026-09-03", &["KL861"]),
                one_way("via-hkg", "AMS", "NRT", "2026-09-03", &["CX270", "CX500"]),
            ],
            Instant::now(),
        );
        let add_seg = AddTripSegmentTool { store: store.clone(), user_id: 7 };
        add_seg
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
        let add_opt =
            AddTripOptionTool { store: store.clone(), user_id: 7, shown: shown.clone(), chat_id: 99 };
        add_opt
            .call(AddOptionArgs {
                trip: "Japan".into(),
                position: 1,
                offer_id: "nonstop".into(),
                decided: Some(true),
            })
            .await
            .unwrap();
        add_opt
            .call(AddOptionArgs {
                trip: "Japan".into(),
                position: 1,
                offer_id: "via-hkg".into(),
                decided: Some(false),
            })
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

        assert_eq!(out.segments[0].also_considered.len(), 1, "the runner-up is priced too");
        assert_eq!(out.segments[0].also_considered[0].flight_numbers, "CX270,CX500");
        assert_eq!(
            out.segments[0].also_considered[0].price_now,
            Some(820.0),
            "priced from the same search as the chosen option, at no extra cost"
        );
    }
}
