//! Building a trip: a named plan the traveller assembles over as many
//! messages as it takes.
//!
//! Nothing here stores an offer id. Offers expire in minutes and a plan
//! outlives the conversation that made it, so a trip holds the itinerary —
//! airports, dates, flight numbers — and finalisation re-prices it.

use crate::store::{TripCandidate, TripSegment};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{TripCandidate, TripSegment};

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
