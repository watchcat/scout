//! Account-scoped trip reads and decisions for channel adapters.
//!
//! A channel may show the durable itinerary and choose between options the
//! traveller already parked. It cannot reach `Store`, invent candidates, or
//! handle live offers: those remain flight-agent responsibilities.

use crate::core::{blocking, Core};
use crate::store::{CandidateChoice, ExpectedSegment, NewCandidate, Trip};

/// A trip plus the same readiness and connection warnings the flight agent
/// sees. One representation keeps chat and the visual client from disagreeing
/// about whether a plan can be priced safely.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Plan {
    #[serde(flatten)]
    pub trip: Trip,
    pub not_ready: Option<String>,
    pub notes: Vec<String>,
}

impl Plan {
    fn from_trip(trip: Trip) -> Self {
        let not_ready = crate::tools::trips::ready_to_price(&trip.segments)
            .err()
            .or_else(|| crate::tools::trips::dates_run_forwards(&trip.segments).err());
        let notes = crate::tools::trips::itinerary_notes(&trip.segments);
        Self {
            trip,
            not_ready,
            notes,
        }
    }
}

/// What a candidate-selection request did. A missing trip and a stale option
/// are ordinary races in a browser tab, not internal errors.
#[derive(Debug, Clone, PartialEq)]
pub enum Selection {
    Chosen(Plan),
    TripNotFound,
    CandidateNotFound,
}

/// Every durable trip for this account, newest activity first.
pub async fn list(core: &Core, account_id: i64) -> anyhow::Result<Vec<Plan>> {
    let store = core.store();
    blocking(move || {
        store
            .list_trips(account_id)
            .map(|trips| trips.into_iter().map(Plan::from_trip).collect())
    })
    .await
}

/// Select one of the already parked candidate flights on a segment.
pub async fn choose(
    core: &Core,
    account_id: i64,
    trip_name: &str,
    position: i64,
    candidate: i64,
) -> anyhow::Result<Selection> {
    let store = core.store();
    let trip_name = trip_name.to_string();
    blocking(move || {
        store
            .choose_candidate_for_account(account_id, &trip_name, position, candidate)
            .map(|outcome| match outcome {
                CandidateChoice::Chosen(trip) => Selection::Chosen(Plan::from_trip(trip)),
                CandidateChoice::TripNotFound => Selection::TripNotFound,
                CandidateChoice::CandidateNotFound => Selection::CandidateNotFound,
            })
    })
    .await
}

/// Records a representative trip without a provider call. This is deliberately
/// named and hidden as test scaffolding: production candidates must come from
/// a flight search so their route, date and quoted price were verified.
#[doc(hidden)]
pub async fn seed_trip_for_tests(core: &Core, account_id: i64, name: &str) -> anyhow::Result<Plan> {
    let store = core.store();
    let name = name.to_string();
    blocking(move || {
        let trip = store.upsert_trip(account_id, &name, Some(2), Some("economy"))?;
        let trip = store.add_segment(trip.id, None, "AMS", "LIS", "2026-10-12")?;
        let trip = store.add_candidate(
            trip.id,
            1,
            ExpectedSegment {
                origin: "AMS",
                destination: "LIS",
                departure_date: Some("2026-10-12"),
            },
            NewCandidate {
                airline: "KLM".to_string(),
                flight_numbers: "KL1579".to_string(),
                itinerary: "AMS 08:20 12.10 ✈ LIS 10:25 12.10".to_string(),
                departing_at_local: Some("2026-10-12T08:20:00".to_string()),
                arriving_at_local: Some("2026-10-12T10:25:00".to_string()),
                duration_minutes: Some(185),
                quoted_price: Some(184.0),
                quoted_currency: Some("EUR".to_string()),
                source: Some("duffel".to_string()),
            },
            false,
        )?;
        let trip = store.add_candidate(
            trip.id,
            1,
            ExpectedSegment {
                origin: "AMS",
                destination: "LIS",
                departure_date: Some("2026-10-12"),
            },
            NewCandidate {
                airline: "TAP Air Portugal".to_string(),
                flight_numbers: "TP673".to_string(),
                itinerary: "AMS 17:40 12.10 ✈ LIS 19:45 12.10".to_string(),
                departing_at_local: Some("2026-10-12T17:40:00".to_string()),
                arriving_at_local: Some("2026-10-12T19:45:00".to_string()),
                duration_minutes: Some(185),
                quoted_price: Some(201.0),
                quoted_currency: Some("EUR".to_string()),
                source: Some("duffel".to_string()),
            },
            false,
        )?;
        Ok(Plan::from_trip(trip))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn core() -> (Core, tempfile::TempDir, i64) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trips-ui.duckdb");
        let core = Core::start(
            crate::config::Config::for_test(path.to_str().unwrap()),
            None,
        )
        .unwrap();
        let account_id = core.store().account_for_telegram(11).unwrap();
        (core, dir, account_id)
    }

    #[tokio::test]
    async fn list_carries_readiness_and_selecting_an_option_updates_it() {
        let (core, _dir, account_id) = core().await;
        seed_trip_for_tests(&core, account_id, "October")
            .await
            .unwrap();

        let before = list(&core, account_id).await.unwrap();
        assert!(before[0]
            .not_ready
            .as_deref()
            .unwrap()
            .contains("2 options"));

        let Selection::Chosen(after) = choose(&core, account_id, "october", 1, 2).await.unwrap()
        else {
            panic!("a stored option was not chosen");
        };
        assert!(after.trip.segments[0].candidates[1].chosen);
        assert!(after.not_ready.is_none());
    }

    #[tokio::test]
    async fn a_trip_cannot_be_selected_through_someone_elses_account() {
        let (core, _dir, owner) = core().await;
        seed_trip_for_tests(&core, owner, "October").await.unwrap();
        let stranger = core.store().account_for_telegram(22).unwrap();

        assert_eq!(
            choose(&core, stranger, "October", 1, 1).await.unwrap(),
            Selection::TripNotFound,
        );
    }
}
