//! Building a trip: a named plan the traveller assembles over as many
//! messages as it takes.
//!
//! Nothing here stores an offer id. Offers expire in minutes and a plan
//! outlives the conversation that made it, so a trip holds the itinerary —
//! airports, dates, flight numbers — and finalisation re-prices it.

use crate::store::{Store, Trip, TripCandidate, TripSegment};
use crate::tools::purchases::{internal, StoreToolError};
use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

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

/// `upsert_trip` and `add_segment` above are two separate lock acquisitions,
/// so a concurrent `delete_trip` landing between them makes `add_segment`
/// fail with a bare "no such trip" — on a tool documented to create the
/// trip when the name is new, which reads as nonsense. Nothing was written
/// either way, so retrying is exactly the right advice; only the wording
/// needed fixing.
fn lost_trip_race(e: anyhow::Error) -> anyhow::Error {
    match e.to_string().as_str() {
        "no such trip" => anyhow::anyhow!(
            "the trip was deleted while this segment was being added — nothing was written, try again"
        ),
        _ => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Store, TripCandidate, TripSegment};
    use rig::tool::Tool;
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
}
