//! Account-scoped trip reads and decisions for channel adapters.
//!
//! A channel may show the durable itinerary and choose between options the
//! traveller already parked. It cannot reach `Store`, invent candidates, or
//! handle live offers: those remain flight-agent responsibilities.

use crate::core::{blocking, Core};
use crate::store::{CandidateChoice, ExpectedSegment, NewCandidate, Trip, TripChat};

/// A trip plus the same readiness and connection warnings the flight agent
/// sees. One representation keeps chat and the visual client from disagreeing
/// about whether a plan can be priced safely.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Plan {
    #[serde(flatten)]
    pub trip: Trip,
    pub not_ready: Option<String>,
    pub notes: Vec<String>,
    /// The chat this trip belongs to. `None` is orphaned — an ordinary
    /// state, reached by outliving the chat that made it.
    pub chat: Option<TripChat>,
}

impl Plan {
    fn from_trip(trip: Trip, chat: Option<TripChat>) -> Self {
        let not_ready = crate::tools::trips::ready_to_price(&trip.segments)
            .err()
            .or_else(|| crate::tools::trips::dates_run_forwards(&trip.segments).err());
        let notes = crate::tools::trips::itinerary_notes(&trip.segments);
        Self {
            trip,
            not_ready,
            notes,
            chat,
        }
    }
}

/// What a candidate-selection request did. A missing trip and a stale option
/// are ordinary races in a browser tab, not internal errors.
#[derive(Debug, Clone, PartialEq)]
pub enum Selection {
    // Boxed: `Plan` grew a `chat` field and tipped this enum over clippy's
    // large-enum-variant threshold against the unit variants beside it.
    Chosen(Box<Plan>),
    TripNotFound,
    CandidateNotFound,
}

/// Every durable trip for this account, newest activity first.
pub async fn list(core: &Core, account_id: i64) -> anyhow::Result<Vec<Plan>> {
    let store = core.store();
    blocking(move || {
        // One `trip_chat` read per trip rather than a second query shape: an
        // account's trip list is the handful of itineraries a traveller is
        // actively planning, not a table a client paginates, so the extra
        // round trips are not worth the JOIN-in-list_trips complexity.
        store
            .list_trips(account_id)?
            .into_iter()
            .map(|trip| {
                let chat = store.trip_chat(trip.id)?;
                Ok(Plan::from_trip(trip, chat))
            })
            .collect()
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
            .and_then(|outcome| match outcome {
                CandidateChoice::Chosen(trip) => {
                    let chat = store.trip_chat(trip.id)?;
                    Ok(Selection::Chosen(Box::new(Plan::from_trip(trip, chat))))
                }
                CandidateChoice::TripNotFound => Ok(Selection::TripNotFound),
                CandidateChoice::CandidateNotFound => Ok(Selection::CandidateNotFound),
            })
    })
    .await
}

/// What a leg edit did. A trip that vanished and a leg that moved are
/// ordinary races in a browser tab, not internal errors.
#[derive(Debug, Clone, PartialEq)]
pub enum LegEdit {
    // Boxed for the same reason as `Selection::Chosen`: one `Plan`-sized
    // variant beside three small ones is what clippy's large_enum_variant
    // objects to, and it objected here too.
    Done(Box<Plan>),
    TripNotFound,
    /// The caller's view of the trip is out of date: the leg it named is no
    /// longer there, or the position it wanted to insert at no longer
    /// exists. One variant for both because they are one thing to the
    /// reader — reload and look again.
    SegmentChanged,
    Invalid(String),
}

/// Add one flight leg to a trip this account already has, appending when
/// `position` is None.
///
/// Validated with the same `iata` and `calendar_date` the model's tools
/// use. Not a second copy of the rules: a client that accepted "Amsterdam"
/// would park a segment no flight search can ever price, and a traveller
/// told different things by the chat and the browser learns there are two
/// products.
pub async fn add_leg(
    core: &Core,
    account_id: i64,
    trip_name: &str,
    position: Option<i64>,
    origin: &str,
    destination: &str,
    departure_date: &str,
) -> anyhow::Result<LegEdit> {
    let (origin, destination) = match crate::tools::trips::leg_ends(origin, destination) {
        Ok(ends) => ends,
        Err(e) => return Ok(LegEdit::Invalid(e.0)),
    };
    let date = match crate::tools::trips::calendar_date(departure_date) {
        Ok(date) => date,
        Err(e) => return Ok(LegEdit::Invalid(e.0)),
    };
    let store = core.store();
    let trip_name = trip_name.to_string();
    blocking(move || {
        // By name and scoped to the account. Trips are addressed by name and
        // two accounts can both have an "Atlantic loop", so an unscoped
        // lookup would let one traveller edit the other's plan.
        let Some(trip) = store.find_trip(account_id, &trip_name)? else {
            return Ok(LegEdit::TripNotFound);
        };
        // A position this trip has nowhere to put is the same failure as a
        // stale remove — the caller's copy of the trip is older than the
        // trip — so it gets the same answer and the reader gets a reload
        // rather than an apology. `add_segment_checked` reports it as a
        // value; nothing here reads the text of an error to find out.
        let Some(trip) =
            store.add_segment_checked(trip.id, position, &origin, &destination, &date)?
        else {
            return Ok(LegEdit::SegmentChanged);
        };
        // Through `trip_chat` rather than assumed: the composer needs the
        // same answer here that `list` gives, or the client would show a
        // trip changing chats when it only gained a leg.
        let chat = store.trip_chat(trip.id)?;
        Ok(LegEdit::Done(Box::new(Plan::from_trip(trip, chat))))
    })
    .await
}

/// Remove one flight leg, but only if it is still the leg the caller drew.
///
/// The route and date are what the client saw, not decoration. Removing a
/// segment renumbers the ones after it, so `position` on its own cannot say
/// which leg a click meant — a tab that rendered the trip before somebody
/// else edited it would name a position that now holds a different flight.
/// `SegmentChanged` is that answer; the client re-reads and asks again.
///
/// `departure_date` is `Option` because a caller may have only a route to
/// go on; `None` means "nothing to verify", not "verified".
pub async fn remove_leg(
    core: &Core,
    account_id: i64,
    trip_name: &str,
    position: i64,
    origin: &str,
    destination: &str,
    departure_date: Option<&str>,
) -> anyhow::Result<LegEdit> {
    let (origin, destination) = match crate::tools::trips::leg_ends(origin, destination) {
        Ok(ends) => ends,
        Err(e) => return Ok(LegEdit::Invalid(e.0)),
    };
    let date = match departure_date.map(crate::tools::trips::calendar_date).transpose() {
        Ok(date) => date,
        Err(e) => return Ok(LegEdit::Invalid(e.0)),
    };
    let store = core.store();
    let trip_name = trip_name.to_string();
    blocking(move || {
        let Some(trip) = store.find_trip(account_id, &trip_name)? else {
            return Ok(LegEdit::TripNotFound);
        };
        let expected = ExpectedSegment {
            origin: &origin,
            destination: &destination,
            departure_date: date.as_deref(),
        };
        // The trip this read against may already be stale by the time the
        // store takes its lock — which is exactly why the expectation is
        // passed down and checked there rather than compared here.
        if !store.remove_segment_checked(trip.id, position, expected)? {
            return Ok(LegEdit::SegmentChanged);
        }
        // Re-read for what to draw. A trip that has gone in the meantime is
        // only reachable by a concurrent delete; the leg did go, but there
        // is no longer a plan to show for it.
        let Some(trip) = store.find_trip(account_id, &trip_name)? else {
            return Ok(LegEdit::TripNotFound);
        };
        let chat = store.trip_chat(trip.id)?;
        Ok(LegEdit::Done(Box::new(Plan::from_trip(trip, chat))))
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
        let trip = store.upsert_trip(account_id, &name, Some(2), Some("economy"), None)?;
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
        // upsert_trip above was called with conversation_id: None, so this
        // trip is orphaned by construction — no store round trip needed to
        // know that.
        Ok(Plan::from_trip(trip, None))
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

    #[tokio::test]
    async fn one_account_cannot_edit_anothers_trip() {
        // Trips are addressed by name, and two accounts can both have a
        // trip called "Atlantic loop". Scoping is what stops one traveller
        // deleting a leg from the other's plan.
        let (core, _dir, owner) = core().await;
        seed_trip_for_tests(&core, owner, "Atlantic loop").await.unwrap();
        let stranger = core.store().account_for_telegram(22).unwrap();
        // The stranger has a trip of the same name, so the lookup has
        // something to find if it ever stops scoping by account.
        seed_trip_for_tests(&core, stranger, "Atlantic loop").await.unwrap();

        let added = add_leg(&core, stranger, "Atlantic loop", None, "LIS", "FCO", "2026-10-14")
            .await
            .unwrap();
        assert!(matches!(added, LegEdit::Done(_)), "got: {added:?}");
        let removed =
            remove_leg(&core, stranger, "Atlantic loop", 1, "AMS", "LIS", Some("2026-10-12"))
                .await
                .unwrap();
        assert!(matches!(removed, LegEdit::Done(_)), "got: {removed:?}");

        let theirs = list(&core, stranger).await.unwrap();
        assert_eq!(
            theirs[0].trip.segments.iter().map(|s| s.destination.as_str()).collect::<Vec<_>>(),
            vec!["FCO"],
            "the stranger's own trip is the one both edits landed on",
        );

        let owned = list(&core, owner).await.unwrap();
        assert_eq!(owned.len(), 1);
        assert_eq!(
            owned[0].trip.segments.iter().map(|s| s.destination.as_str()).collect::<Vec<_>>(),
            vec!["LIS"],
            "neither edit reached the other account's trip",
        );
    }

    #[tokio::test]
    async fn a_leg_edit_on_a_trip_this_account_does_not_have_finds_no_trip() {
        let (core, _dir, owner) = core().await;
        seed_trip_for_tests(&core, owner, "Atlantic loop").await.unwrap();
        let stranger = core.store().account_for_telegram(22).unwrap();

        assert_eq!(
            add_leg(&core, stranger, "Atlantic loop", None, "LIS", "FCO", "2026-10-14")
                .await
                .unwrap(),
            LegEdit::TripNotFound,
        );
        assert_eq!(
            remove_leg(&core, stranger, "Atlantic loop", 1, "AMS", "LIS", Some("2026-10-12"))
                .await
                .unwrap(),
            LegEdit::TripNotFound,
        );

        let owned = list(&core, owner).await.unwrap();
        assert_eq!(owned[0].trip.segments.len(), 1, "the owner's plan is untouched");
        assert!(!owned[0].trip.segments[0].candidates.is_empty(), "and so are its options");
    }

    #[tokio::test]
    async fn adding_a_leg_and_removing_it_again_leaves_the_plan_where_it_started() {
        let (core, _dir, account_id) = core().await;
        let store = core.store();
        let conversation_id = store.start_conversation(account_id, "direct").unwrap();
        store
            .upsert_trip(account_id, "Atlantic loop", Some(2), Some("economy"), Some(conversation_id))
            .unwrap();
        add_leg(&core, account_id, "Atlantic loop", None, "AMS", "LIS", "2026-10-12")
            .await
            .unwrap();

        let LegEdit::Done(added) =
            add_leg(&core, account_id, "atlantic loop", None, "lis", "fco", "2026-10-14")
                .await
                .unwrap()
        else {
            panic!("a valid leg was not added");
        };
        assert_eq!(
            added.trip.segments.iter().map(|s| (s.position, s.origin.as_str())).collect::<Vec<_>>(),
            vec![(1, "AMS"), (2, "LIS")],
            "codes are upper-cased and the trip is found by a lower-cased name",
        );
        assert_eq!(
            added.chat.as_ref().map(|c| c.id),
            Some(conversation_id),
            "an edited plan still names the chat it belongs to",
        );

        let LegEdit::Done(removed) =
            remove_leg(&core, account_id, "Atlantic loop", 2, "LIS", "FCO", Some("2026-10-14"))
                .await
                .unwrap()
        else {
            panic!("a leg that matched what the caller saw was not removed");
        };
        assert_eq!(removed.trip.segments.len(), 1);
        assert_eq!(removed.chat.map(|c| c.id), Some(conversation_id));
    }

    #[tokio::test]
    async fn a_leg_removed_at_a_position_that_has_since_moved_reports_the_change() {
        // The browser drew two legs and someone removed the first, so the
        // second is now position 1. The stale click must not take it.
        let (core, _dir, account_id) = core().await;
        core.store()
            .upsert_trip(account_id, "Atlantic loop", None, None, None)
            .unwrap();
        add_leg(&core, account_id, "Atlantic loop", None, "AMS", "LIS", "2026-10-12")
            .await
            .unwrap();
        add_leg(&core, account_id, "Atlantic loop", None, "LIS", "FCO", "2026-10-14")
            .await
            .unwrap();
        remove_leg(&core, account_id, "Atlantic loop", 1, "AMS", "LIS", Some("2026-10-12"))
            .await
            .unwrap();

        assert_eq!(
            remove_leg(&core, account_id, "Atlantic loop", 2, "LIS", "FCO", Some("2026-10-14"))
                .await
                .unwrap(),
            LegEdit::SegmentChanged,
            "a position that no longer exists is a stale tab, not an error",
        );
        assert_eq!(
            remove_leg(&core, account_id, "Atlantic loop", 1, "AMS", "LIS", Some("2026-10-12"))
                .await
                .unwrap(),
            LegEdit::SegmentChanged,
            "and neither is a position the renumber refilled with something else",
        );

        let plans = list(&core, account_id).await.unwrap();
        assert_eq!(plans[0].trip.segments.len(), 1);
        assert_eq!(plans[0].trip.segments[0].destination, "FCO", "the surviving leg is untouched");
    }

    #[tokio::test]
    async fn a_leg_inserted_at_a_position_the_trip_no_longer_has_reports_the_change() {
        // A tab that drew four legs asking to insert at 4 on a trip now two
        // legs long is the same failure as a stale remove: its copy is older
        // than the trip. Reload, not an apology and not an ERROR line.
        let (core, _dir, account_id) = core().await;
        core.store()
            .upsert_trip(account_id, "Atlantic loop", None, None, None)
            .unwrap();
        add_leg(&core, account_id, "Atlantic loop", None, "AMS", "LIS", "2026-10-12")
            .await
            .unwrap();
        add_leg(&core, account_id, "Atlantic loop", None, "LIS", "FCO", "2026-10-14")
            .await
            .unwrap();

        assert_eq!(
            add_leg(&core, account_id, "Atlantic loop", Some(4), "BCN", "MAD", "2026-10-13")
                .await
                .unwrap(),
            LegEdit::SegmentChanged,
        );

        let plans = list(&core, account_id).await.unwrap();
        assert_eq!(
            plans[0].trip.segments.iter().map(|s| s.origin.as_str()).collect::<Vec<_>>(),
            vec!["AMS", "LIS"],
            "the trip is exactly as it was",
        );
        // The last place a leg can go is still open: the refusal is about
        // position 4 on this trip, not about inserting at all.
        assert!(matches!(
            add_leg(&core, account_id, "Atlantic loop", Some(3), "FCO", "AMS", "2026-10-18")
                .await
                .unwrap(),
            LegEdit::Done(_),
        ));
    }

    #[tokio::test]
    async fn a_leg_added_at_a_position_that_exists_goes_in_front_of_it_rather_than_over_it() {
        // Overwriting would lose a leg the traveller never asked to lose,
        // and lose it on the path with no confirmation step.
        let (core, _dir, account_id) = core().await;
        seed_trip_for_tests(&core, account_id, "Atlantic loop").await.unwrap();
        add_leg(&core, account_id, "Atlantic loop", None, "LIS", "FCO", "2026-10-14")
            .await
            .unwrap();

        let LegEdit::Done(plan) =
            add_leg(&core, account_id, "Atlantic loop", Some(1), "BCN", "MAD", "2026-10-10")
                .await
                .unwrap()
        else {
            panic!("a valid insert was refused");
        };
        assert_eq!(
            plan.trip
                .segments
                .iter()
                .map(|s| (s.position, s.origin.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "BCN"), (2, "AMS"), (3, "LIS")],
            "inserting at 1 shifts the rest down; nothing is overwritten",
        );
        // The parked options moved with their segment rather than staying
        // on position 1 and reattaching to a route nobody quoted them for.
        assert!(plan.trip.segments[0].candidates.is_empty());
        assert_eq!(plan.trip.segments[1].candidates.len(), 2);
    }

    #[tokio::test]
    async fn a_leg_the_flight_search_could_never_price_is_refused_before_the_store() {
        let (core, _dir, account_id) = core().await;
        core.store()
            .upsert_trip(account_id, "Atlantic loop", None, None, None)
            .unwrap();

        let refusals = [
            add_leg(&core, account_id, "Atlantic loop", None, "Amsterdam", "FCO", "2026-10-14")
                .await
                .unwrap(),
            add_leg(&core, account_id, "Atlantic loop", None, "LIS", "LIS", "2026-10-14")
                .await
                .unwrap(),
            add_leg(&core, account_id, "Atlantic loop", None, "LIS", "FCO", "14/10/2026")
                .await
                .unwrap(),
            remove_leg(&core, account_id, "Atlantic loop", 1, "Amsterdam", "FCO", None)
                .await
                .unwrap(),
        ];
        for refusal in &refusals {
            assert!(matches!(refusal, LegEdit::Invalid(_)), "got: {refusal:?}");
        }
        let LegEdit::Invalid(same_place) = &refusals[1] else { unreachable!() };
        assert!(
            same_place.contains("a flight needs two different places"),
            "the client refuses in the same words the model's tools do, got: {same_place}",
        );

        let plans = list(&core, account_id).await.unwrap();
        assert!(plans[0].trip.segments.is_empty(), "nothing reached the store");
    }

    #[tokio::test]
    async fn a_plan_names_the_chat_it_belongs_to() {
        // The composer has to say where a message will land. An orphan says
        // so by carrying None, which the client renders as "a new chat".
        let (core, _dir, account_id) = core().await;
        let store = core.store();
        let conversation_id = store.start_conversation(account_id, "direct").unwrap();
        store
            .set_thread_title(account_id, conversation_id, "Cheap flights in October")
            .unwrap();
        store
            .upsert_trip(account_id, "October", Some(2), Some("economy"), Some(conversation_id))
            .unwrap();

        let plans = list(&core, account_id).await.unwrap();
        let chat = plans[0]
            .chat
            .as_ref()
            .expect("a trip made in a chat must name it on the plan");
        assert_eq!(chat.id, conversation_id);
        assert_eq!(chat.title.as_deref(), Some("Cheap flights in October"));
        assert_eq!(chat.scope, "direct");
    }

    #[tokio::test]
    async fn a_trip_that_outlived_its_chat_names_no_chat() {
        // `conversation_id IS NULL` — and, via the JOIN in `trip_chat`, a
        // `conversation_id` pointing nowhere — must both read as `None`, not
        // an error and not a struct with some fields missing.
        let (core, _dir, account_id) = core().await;
        seed_trip_for_tests(&core, account_id, "October").await.unwrap();

        let plans = list(&core, account_id).await.unwrap();
        assert_eq!(plans[0].chat, None);
    }

    #[tokio::test]
    async fn a_plans_chat_serializes_the_way_the_web_client_expects() {
        let (core, _dir, account_id) = core().await;
        let store = core.store();
        let conversation_id = store.start_conversation(account_id, "direct").unwrap();
        store
            .set_thread_title(account_id, conversation_id, "Cheap flights in October")
            .unwrap();
        store
            .upsert_trip(account_id, "October", Some(2), Some("economy"), Some(conversation_id))
            .unwrap();
        seed_trip_for_tests(&core, account_id, "Orphaned").await.unwrap();

        let plans = list(&core, account_id).await.unwrap();
        let owned = plans.iter().find(|p| p.trip.name == "October").unwrap();
        let orphaned = plans.iter().find(|p| p.trip.name == "Orphaned").unwrap();

        assert_eq!(
            serde_json::to_value(owned).unwrap()["chat"],
            serde_json::json!({
                "id": conversation_id,
                "title": "Cheap flights in October",
                "scope": "direct",
            }),
        );
        assert_eq!(
            serde_json::to_value(orphaned).unwrap()["chat"],
            serde_json::Value::Null,
        );
    }
}
