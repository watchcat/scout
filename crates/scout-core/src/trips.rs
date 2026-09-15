//! Account-scoped trip reads and decisions for channel adapters.
//!
//! A channel may show the durable itinerary and choose between options the
//! traveller already parked. It cannot reach `Store`, invent candidates, or
//! handle live offers: those remain flight-agent responsibilities.

use crate::core::{blocking, Core};
use crate::store::{CandidateChoice, ExpectedItem, NewCandidate, NewItem, TripChat};
pub use crate::store::{Trip, TripCandidate, TripItem};

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
    pub(crate) fn from_trip(trip: Trip, chat: Option<TripChat>) -> Self {
        let not_ready = crate::tools::trips::ready_to_price(&trip.items)
            .err()
            .or_else(|| crate::tools::trips::dates_run_forwards(&trip.items).err());
        let notes = crate::tools::trips::itinerary_notes(&trip.items);
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

/// Every trip for this account, newest activity first — drafts included.
///
/// Drafts were hidden here for a day, so that a casual price check would not
/// litter the Trips tab. It cost more than it saved: having watched the
/// specialist build a trip, the traveller could not tell "built but
/// invisible" from "not built at all", which is the complaint this whole
/// feature came from. A draft is shown and marked instead — `trip.kept`
/// rides along on the plan for the client to mark it with.
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

/// One durable trip by the name the traveller gave it, scoped to their
/// account. The store performs the same case-insensitive lookup as chat.
///
/// A draft answers like any other trip. It briefly did not, while `list`
/// hid drafts and this had to agree with it; now that the Trips tab shows a
/// draft, refusing it here would offer the traveller a PDF of a trip and
/// then decline to render it.
pub async fn find(core: &Core, account_id: i64, name: &str) -> anyhow::Result<Option<Plan>> {
    let store = core.store();
    let name = name.to_string();
    blocking(move || {
        store
            .find_trip(account_id, &name)
            .and_then(|trip| match trip {
                Some(trip) => {
                    let chat = store.trip_chat(trip.id)?;
                    Ok(Some(Plan::from_trip(trip, chat)))
                }
                None => Ok(None),
            })
    })
    .await
}

/// Keep one of this account's trips, so it outlives the chat that built it.
///
/// `Option<Plan>` rather than an enum of its own, because keeping has the
/// same two outcomes `find` has: the trip is there or the name is not this
/// account's. Keeping an already-kept trip is one of the successes — a
/// traveller pressing Keep twice is repeating themselves, not erring.
///
/// The plan comes back so a browser can repaint from the answer rather than
/// fetching the list again, like every other trip write here.
pub async fn keep(core: &Core, account_id: i64, name: &str) -> anyhow::Result<Option<Plan>> {
    let store = core.store();
    let name = name.to_string();
    blocking(move || {
        if !store.keep_trip(account_id, &name)? {
            return Ok(None);
        }
        // Re-read rather than flipping `kept` on a copy: keeping bumps
        // `updated_at`, and this plan is what the client redraws from.
        store
            .find_trip(account_id, &name)
            .and_then(|trip| match trip {
                Some(trip) => {
                    let chat = store.trip_chat(trip.id)?;
                    Ok(Some(Plan::from_trip(trip, chat)))
                }
                // Only reachable by a concurrent delete between the two
                // statements: it really was kept, there is just no longer a
                // plan to show for it.
                None => Ok(None),
            })
    })
    .await
}

/// Delete one of this account's trips, and everything parked on it.
///
/// `Option` rather than an enum of its own for the same reason `keep` uses
/// one: there are two outcomes, and "the name is not this account's" is the
/// second. `None` covers both a name nobody used and another account's
/// trip, which is what keeps a caller from learning that someone else's
/// "Atlantic loop" exists.
///
/// `Some` carries the trips that are *left*, not the trip that went. Every
/// other write here answers with the plan to repaint; a deleted trip has no
/// plan, and what a client needs next is which trips remain and which of
/// them to show — the same list a reload would have given it, so the two
/// paths cannot disagree about what an account has.
///
/// `delete_trip` takes the trip's segments and its parked candidates with
/// it. That cascade is the point rather than a detail: both tables are
/// reached only by `trip_id`, so a row left behind after the trip is gone
/// is unreachable by every read path there is and stays in the database
/// forever.
pub async fn delete(core: &Core, account_id: i64, name: &str) -> anyhow::Result<Option<Vec<Plan>>> {
    let store = core.store();
    let owned = name.to_string();
    // Scoped to the account in the store, like every other lookup by name
    // here: two accounts can both have an "Atlantic loop", and this is the
    // one write where reaching the wrong one destroys it.
    if !blocking(move || store.delete_trip(account_id, &owned)).await? {
        return Ok(None);
    }
    // Through `list` rather than a second query shape, so a client that
    // repaints from this answer sees exactly what reloading would show.
    list(core, account_id).await.map(Some)
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
    /// The caller's view of the trip is out of date: the item it named is
    /// no longer at that position, or is not the item it described. The
    /// name is the one the client has always matched on; what it means to
    /// the reader is unchanged — reload and look again.
    SegmentChanged,
    Invalid(String),
}

/// Add one flight leg to a trip this account already has. It lands where
/// its date puts it: positions follow dates on every write, so there is no
/// position to ask for.
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
    origin: &str,
    destination: &str,
    departure_date: &str,
) -> anyhow::Result<LegEdit> {
    let (origin, destination) = match crate::tools::trips::leg_ends(origin, destination) {
        Ok(ends) => ends,
        Err(e) => return Ok(LegEdit::Invalid(e.0)),
    };
    let date = match crate::tools::trips::calendar_date("departure_date", departure_date) {
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
        // `None` here is a trip deleted between the lookup above and the
        // write: the same "not this account's trip" the lookup would have
        // given a moment later. `add_flight_checked` reports it as a value;
        // nothing here reads the text of an error to find out.
        let Some(trip) = store.add_flight_checked(trip.id, &origin, &destination, &date)? else {
            return Ok(LegEdit::TripNotFound);
        };
        // Through `trip_chat` rather than assumed: the composer needs the
        // same answer here that `list` gives, or the client would show a
        // trip changing chats when it only gained a leg.
        let chat = store.trip_chat(trip.id)?;
        Ok(LegEdit::Done(Box::new(Plan::from_trip(trip, chat))))
    })
    .await
}

/// What the client saw on the item it is asking to remove. A flight card
/// sends its route, a stay sends its title; both may add the date. Every
/// `Some` must match the row — `None` means "nothing to verify", not
/// "verified" — so a caller with only a route to go on is not quietly
/// granted a free pass on the date. The same goes end by end: a client that
/// sends one end of a route and not the other checks less, not nothing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RemoveExpectation {
    pub origin: Option<String>,
    pub destination: Option<String>,
    pub title: Option<String>,
    pub date: Option<String>,
}

/// Remove one item, but only if it is still the item the caller drew.
///
/// The expectation is not decoration. Removing an item renumbers the ones
/// after it, so `position` on its own cannot say which item a click meant —
/// a tab that rendered the trip before somebody else edited it would name
/// a position that now holds a different thing. `SegmentChanged` is that
/// answer; the client re-reads and asks again.
///
/// The route goes through the same `leg_ends` the flight tools use and the
/// date through the same `calendar_date`, so a value the client could not
/// have drawn is refused as `Invalid` rather than silently failing to match.
pub async fn remove_item(
    core: &Core,
    account_id: i64,
    trip_name: &str,
    position: i64,
    expected: RemoveExpectation,
) -> anyhow::Result<LegEdit> {
    let (origin, destination) = match (expected.origin, expected.destination) {
        (Some(origin), Some(destination)) => {
            match crate::tools::trips::leg_ends(&origin, &destination) {
                Ok((origin, destination)) => (Some(origin), Some(destination)),
                Err(e) => return Ok(LegEdit::Invalid(e.0)),
            }
        }
        // One end on its own cannot be checked against the other, but it
        // still has to be a code a flight could carry.
        (origin, destination) => {
            let end = |label, value: Option<String>| {
                value
                    .map(|v| crate::tools::trips::iata(label, &v))
                    .transpose()
            };
            match (end("origin", origin), end("destination", destination)) {
                (Ok(origin), Ok(destination)) => (origin, destination),
                (Err(e), _) | (_, Err(e)) => return Ok(LegEdit::Invalid(e.0)),
            }
        }
    };
    let date = match expected
        .date
        .as_deref()
        .map(|d| crate::tools::trips::calendar_date("date", d))
        .transpose()
    {
        Ok(date) => date,
        Err(e) => return Ok(LegEdit::Invalid(e.0)),
    };
    let title = expected.title;
    let store = core.store();
    let trip_name = trip_name.to_string();
    blocking(move || {
        let Some(trip) = store.find_trip(account_id, &trip_name)? else {
            return Ok(LegEdit::TripNotFound);
        };
        let expected = ExpectedItem {
            origin: origin.as_deref(),
            destination: destination.as_deref(),
            title: title.as_deref(),
            date: date.as_deref(),
        };
        // The trip this read against may already be stale by the time the
        // store takes its lock — which is exactly why the expectation is
        // passed down and checked there rather than compared here.
        if !store.remove_item_checked(trip.id, position, expected)? {
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
        let trip = store.add_flight(trip.id, "AMS", "LIS", "2026-10-12")?;
        let trip = store.add_candidate(
            trip.id,
            1,
            ExpectedItem {
                origin: Some("AMS"),
                destination: Some("LIS"),
                title: None,
                date: Some("2026-10-12"),
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
            ExpectedItem {
                origin: Some("AMS"),
                destination: Some("LIS"),
                title: None,
                date: Some("2026-10-12"),
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
        // Left a draft, which is what a flight search actually produces.
        // This used to keep the trip on every caller's behalf, because
        // `list` then hid drafts and no fixture would have been visible;
        // `list` shows them again, and keeping here would hide the one
        // state the keep route has to be tested against.
        // upsert_trip above was called with conversation_id: None, so this
        // trip is orphaned by construction — no store round trip needed to
        // know that.
        Ok(Plan::from_trip(trip, None))
    })
    .await
}

/// Puts a stay, an activity or a transport booking on a trip this account
/// already has, without going through chat. Test scaffolding like
/// `seed_trip_for_tests`: production items arrive through the model's
/// `add_trip_item` tool, which is where their fields are validated.
#[doc(hidden)]
pub async fn seed_item_for_tests(
    core: &Core,
    account_id: i64,
    trip_name: &str,
    kind: &str,
    title: &str,
    date: &str,
) -> anyhow::Result<Plan> {
    let store = core.store();
    let trip_name = trip_name.to_string();
    let item = NewItem {
        kind: kind.to_string(),
        title: title.to_string(),
        place: None,
        date: date.to_string(),
        starts_at: None,
        ends_at: None,
        notes: None,
        booked: false,
        confirmation_code: None,
        price: None,
        currency: None,
        arrival_id: None,
    };
    blocking(move || {
        let Some(trip) = store.find_trip(account_id, &trip_name)? else {
            anyhow::bail!("seed_item_for_tests: no trip {trip_name:?} for account {account_id}");
        };
        let trip = store.add_item(trip.id, item)?;
        let chat = store.trip_chat(trip.id)?;
        Ok(Plan::from_trip(trip, chat))
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

    /// What a client that drew a flight leg sends back to remove it.
    fn flight(origin: &str, destination: &str, date: Option<&str>) -> RemoveExpectation {
        RemoveExpectation {
            origin: Some(origin.to_string()),
            destination: Some(destination.to_string()),
            title: None,
            date: date.map(str::to_string),
        }
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
        assert!(after.trip.items[0].candidates[1].chosen);
        assert!(after.not_ready.is_none());
    }

    #[tokio::test]
    async fn the_trips_tab_shows_the_drafts_built_while_searching_and_marks_them() {
        // Hiding drafts here was tried for a day and reversed. It stopped
        // casual price checks littering the tab, but it also meant a
        // traveller who had just watched the specialist build a trip could
        // not tell "built but invisible" from "not built at all" — the
        // complaint the Trips tab exists to answer.
        let (core, _dir, account_id) = core().await;
        core.store()
            .upsert_trip(account_id, "Just browsing", None, None, None)
            .unwrap();
        seed_trip_for_tests(&core, account_id, "October").await.unwrap();
        core.store().keep_trip(account_id, "October").unwrap();

        let plans = list(&core, account_id).await.unwrap();
        assert_eq!(
            plans.iter().map(|p| (p.trip.name.as_str(), p.trip.kept)).collect::<Vec<_>>(),
            vec![("October", true), ("Just browsing", false)],
            "the draft reaches the traveller, saying it is a draft",
        );
        // The mark has to survive serialization or the browser cannot draw
        // the difference, and a draft becomes indistinguishable again.
        let draft = serde_json::to_value(&plans[1]).unwrap();
        assert_eq!(draft["kept"], serde_json::json!(false));
        assert_eq!(
            serde_json::to_value(&plans[0]).unwrap()["kept"],
            serde_json::json!(true),
        );
    }

    #[tokio::test]
    async fn a_draft_is_fetched_by_name_like_any_other_trip() {
        // `find` is what the PDF route goes through. While drafts were
        // hidden it refused them, which now would mean a trip the tab shows
        // and an export that declines to render it.
        let (core, _dir, account_id) = core().await;
        seed_trip_for_tests(&core, account_id, "October")
            .await
            .unwrap();

        let plan = find(&core, account_id, "october").await.unwrap().unwrap();
        assert!(!plan.trip.kept, "the trip a flight search builds is a draft");
        assert_eq!(plan.trip.items.len(), 1);
        assert_eq!(
            find(&core, account_id, "a trip nobody made").await.unwrap(),
            None,
            "a name that was never used is still nothing",
        );
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
        seed_trip_for_tests(&core, owner, "Atlantic loop")
            .await
            .unwrap();
        let stranger = core.store().account_for_telegram(22).unwrap();
        // The stranger has a trip of the same name, so the lookup has
        // something to find if it ever stops scoping by account.
        seed_trip_for_tests(&core, stranger, "Atlantic loop")
            .await
            .unwrap();

        let added = add_leg(&core, stranger, "Atlantic loop", "LIS", "FCO", "2026-10-14")
            .await
            .unwrap();
        assert!(matches!(added, LegEdit::Done(_)), "got: {added:?}");
        let removed = remove_item(
            &core,
            stranger,
            "Atlantic loop",
            1,
            flight("AMS", "LIS", Some("2026-10-12")),
        )
        .await
        .unwrap();
        assert!(matches!(removed, LegEdit::Done(_)), "got: {removed:?}");

        let theirs = list(&core, stranger).await.unwrap();
        assert_eq!(
            theirs[0]
                .trip
                .items
                .iter()
                .map(|s| s.destination.as_deref().unwrap_or(""))
                .collect::<Vec<_>>(),
            vec!["FCO"],
            "the stranger's own trip is the one both edits landed on",
        );

        let owned = list(&core, owner).await.unwrap();
        assert_eq!(owned.len(), 1);
        assert_eq!(
            owned[0]
                .trip
                .items
                .iter()
                .map(|s| s.destination.as_deref().unwrap_or(""))
                .collect::<Vec<_>>(),
            vec!["LIS"],
            "neither edit reached the other account's trip",
        );
    }

    #[tokio::test]
    async fn deleting_a_trip_takes_it_off_the_list_and_answers_with_what_is_left() {
        let (core, _dir, account_id) = core().await;
        seed_trip_for_tests(&core, account_id, "October").await.unwrap();
        seed_trip_for_tests(&core, account_id, "Atlantic loop").await.unwrap();

        let left = delete(&core, account_id, "october")
            .await
            .unwrap()
            .expect("a trip this account has is deleted, found by a lower-cased name");
        assert_eq!(
            left.iter().map(|p| p.trip.name.as_str()).collect::<Vec<_>>(),
            vec!["Atlantic loop"],
            "the answer is what the tab should now show, not the trip that went",
        );
        assert_eq!(
            list(&core, account_id).await.unwrap().len(),
            1,
            "and it agrees with what a reload would have given",
        );
    }

    #[tokio::test]
    async fn deleting_a_trip_this_account_does_not_have_deletes_nothing() {
        let (core, _dir, account_id) = core().await;
        seed_trip_for_tests(&core, account_id, "October").await.unwrap();

        assert_eq!(
            delete(&core, account_id, "a trip nobody made").await.unwrap(),
            None,
            "a name nobody used is nothing to delete, not an error",
        );
        assert_eq!(list(&core, account_id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn one_account_cannot_delete_anothers_trip_of_the_same_name() {
        // The whole-trip delete is the one write where reaching across
        // accounts destroys rather than edits: the owner would lose every
        // leg and every parked option with no press of their own.
        let (core, _dir, owner) = core().await;
        seed_trip_for_tests(&core, owner, "Atlantic loop").await.unwrap();
        let stranger = core.store().account_for_telegram(22).unwrap();
        // The stranger has one of the same name, so the lookup has something
        // to find if it ever stops scoping by account — and deleting theirs
        // must not be what deletes the owner's.
        seed_trip_for_tests(&core, stranger, "Atlantic loop").await.unwrap();

        let left = delete(&core, stranger, "Atlantic loop").await.unwrap().unwrap();
        assert!(left.is_empty(), "the stranger deleted their own and has none left");

        let owned = list(&core, owner).await.unwrap();
        assert_eq!(owned.len(), 1, "the owner's trip of the same name is untouched");
        assert_eq!(owned[0].trip.items.len(), 1);
        assert_eq!(
            owned[0].trip.items[0].candidates.len(),
            2,
            "and so are the options parked on it",
        );

        // Now that the stranger has none, the name is not theirs to delete.
        assert_eq!(delete(&core, stranger, "Atlantic loop").await.unwrap(), None);
        assert_eq!(list(&core, owner).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_leg_edit_on_a_trip_this_account_does_not_have_finds_no_trip() {
        let (core, _dir, owner) = core().await;
        seed_trip_for_tests(&core, owner, "Atlantic loop")
            .await
            .unwrap();
        let stranger = core.store().account_for_telegram(22).unwrap();

        assert_eq!(
            add_leg(&core, stranger, "Atlantic loop", "LIS", "FCO", "2026-10-14")
                .await
                .unwrap(),
            LegEdit::TripNotFound,
        );
        assert_eq!(
            remove_item(
                &core,
                stranger,
                "Atlantic loop",
                1,
                flight("AMS", "LIS", Some("2026-10-12")),
            )
            .await
            .unwrap(),
            LegEdit::TripNotFound,
        );

        let owned = list(&core, owner).await.unwrap();
        assert_eq!(
            owned[0].trip.items.len(),
            1,
            "the owner's plan is untouched"
        );
        assert!(
            !owned[0].trip.items[0].candidates.is_empty(),
            "and so are its options"
        );
    }

    #[tokio::test]
    async fn adding_a_leg_and_removing_it_again_leaves_the_plan_where_it_started() {
        let (core, _dir, account_id) = core().await;
        let store = core.store();
        let conversation_id = store.start_conversation(account_id, "direct").unwrap();
        store
            .upsert_trip(
                account_id,
                "Atlantic loop",
                Some(2),
                Some("economy"),
                Some(conversation_id),
            )
            .unwrap();
        add_leg(&core, account_id, "Atlantic loop", "AMS", "LIS", "2026-10-12")
            .await
            .unwrap();

        let LegEdit::Done(added) =
            add_leg(&core, account_id, "atlantic loop", "lis", "fco", "2026-10-14")
                .await
                .unwrap()
        else {
            panic!("a valid leg was not added");
        };
        assert_eq!(
            added
                .trip
                .items
                .iter()
                .map(|s| (s.position, s.origin.as_deref().unwrap_or("")))
                .collect::<Vec<_>>(),
            vec![(1, "AMS"), (2, "LIS")],
            "codes are upper-cased and the trip is found by a lower-cased name",
        );
        assert_eq!(
            added.chat.as_ref().map(|c| c.id),
            Some(conversation_id),
            "an edited plan still names the chat it belongs to",
        );

        let LegEdit::Done(removed) = remove_item(
            &core,
            account_id,
            "Atlantic loop",
            2,
            flight("LIS", "FCO", Some("2026-10-14")),
        )
        .await
        .unwrap() else {
            panic!("a leg that matched what the caller saw was not removed");
        };
        assert_eq!(removed.trip.items.len(), 1);
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
        // This test is about a stale position, not about keeping — kept
        // explicitly so the `list` read at the bottom can see it.
        core.store().keep_trip(account_id, "Atlantic loop").unwrap();
        add_leg(&core, account_id, "Atlantic loop", "AMS", "LIS", "2026-10-12")
            .await
            .unwrap();
        add_leg(&core, account_id, "Atlantic loop", "LIS", "FCO", "2026-10-14")
            .await
            .unwrap();
        remove_item(
            &core,
            account_id,
            "Atlantic loop",
            1,
            flight("AMS", "LIS", Some("2026-10-12")),
        )
        .await
        .unwrap();

        assert_eq!(
            remove_item(
                &core,
                account_id,
                "Atlantic loop",
                2,
                flight("LIS", "FCO", Some("2026-10-14")),
            )
            .await
            .unwrap(),
            LegEdit::SegmentChanged,
            "a position that no longer exists is a stale tab, not an error",
        );
        assert_eq!(
            remove_item(
                &core,
                account_id,
                "Atlantic loop",
                1,
                flight("AMS", "LIS", Some("2026-10-12")),
            )
            .await
            .unwrap(),
            LegEdit::SegmentChanged,
            "and neither is a position the renumber refilled with something else",
        );

        let plans = list(&core, account_id).await.unwrap();
        assert_eq!(plans[0].trip.items.len(), 1);
        assert_eq!(
            plans[0].trip.items[0].destination.as_deref(), Some("FCO"),
            "the surviving leg is untouched"
        );
    }

    #[tokio::test]
    async fn a_leg_goes_where_its_date_puts_it_and_the_options_stay_with_their_leg() {
        // There is no position to ask for any more: an earlier date lands in
        // front, and the legs already there move down rather than being
        // written over — which would lose a leg the traveller never asked
        // to lose, on the path with no confirmation step.
        let (core, _dir, account_id) = core().await;
        seed_trip_for_tests(&core, account_id, "Atlantic loop")
            .await
            .unwrap();
        add_leg(&core, account_id, "Atlantic loop", "LIS", "FCO", "2026-10-14")
            .await
            .unwrap();

        let LegEdit::Done(plan) =
            add_leg(&core, account_id, "Atlantic loop", "BCN", "MAD", "2026-10-10")
                .await
                .unwrap()
        else {
            panic!("a valid leg was refused");
        };
        assert_eq!(
            plan.trip
                .items
                .iter()
                .map(|s| (s.position, s.origin.as_deref().unwrap_or("")))
                .collect::<Vec<_>>(),
            vec![(1, "BCN"), (2, "AMS"), (3, "LIS")],
            "the earliest date is first; nothing is overwritten",
        );
        // The parked options moved with their leg rather than staying on
        // position 1 and reattaching to a route nobody quoted them for.
        assert!(plan.trip.items[0].candidates.is_empty());
        assert_eq!(plan.trip.items[1].candidates.len(), 2);
    }

    #[tokio::test]
    async fn a_stay_is_removed_by_its_title_and_a_wrong_title_removes_nothing() {
        // A stay has no route, so its title is what the client saw and what
        // the guard checks. The wrong title is a tab whose copy of the trip
        // is older than the trip: reload, not a deletion.
        let (core, _dir, account_id) = core().await;
        seed_trip_for_tests(&core, account_id, "October")
            .await
            .unwrap();
        // Same day as the flight, so the stay sorts behind it: position 2.
        let seeded =
            seed_item_for_tests(&core, account_id, "October", "stay", "Hotel Lisboa", "2026-10-12")
                .await
                .unwrap();
        assert_eq!(
            seeded
                .trip
                .items
                .iter()
                .map(|i| (i.position, i.kind.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "flight"), (2, "stay")],
        );

        assert_eq!(
            remove_item(
                &core,
                account_id,
                "October",
                2,
                RemoveExpectation {
                    title: Some("Hostel Lisboa".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
            LegEdit::SegmentChanged,
            "a title that is not what is there is a stale tab, not a match",
        );
        let LegEdit::Done(plan) = remove_item(
            &core,
            account_id,
            "October",
            2,
            RemoveExpectation {
                title: Some("Hotel Lisboa".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap() else {
            panic!("a stay that matched its title was not removed");
        };
        assert_eq!(
            plan.trip.items.iter().map(|i| i.kind.as_str()).collect::<Vec<_>>(),
            vec!["flight"],
            "the stay went and the flight beside it did not",
        );
        assert_eq!(plan.trip.items[0].candidates.len(), 2);
    }

    #[tokio::test]
    async fn a_leg_the_flight_search_could_never_price_is_refused_before_the_store() {
        let (core, _dir, account_id) = core().await;
        core.store()
            .upsert_trip(account_id, "Atlantic loop", None, None, None)
            .unwrap();
        // This test is about refused edits, not about keeping — kept
        // explicitly so the `list` read at the bottom can see it.
        core.store().keep_trip(account_id, "Atlantic loop").unwrap();

        let refusals = [
            add_leg(&core, account_id, "Atlantic loop", "Amsterdam", "FCO", "2026-10-14")
                .await
                .unwrap(),
            add_leg(&core, account_id, "Atlantic loop", "LIS", "LIS", "2026-10-14")
                .await
                .unwrap(),
            add_leg(&core, account_id, "Atlantic loop", "LIS", "FCO", "14/10/2026")
                .await
                .unwrap(),
            remove_item(
                &core,
                account_id,
                "Atlantic loop",
                1,
                flight("Amsterdam", "FCO", None),
            )
            .await
            .unwrap(),
            // A date the client could not have drawn is refused the same
            // way, whichever kind of item it is checking.
            remove_item(
                &core,
                account_id,
                "Atlantic loop",
                1,
                RemoveExpectation {
                    title: Some("Hotel Lisboa".to_string()),
                    date: Some("14/10/2026".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
        ];
        for refusal in &refusals {
            assert!(matches!(refusal, LegEdit::Invalid(_)), "got: {refusal:?}");
        }
        let LegEdit::Invalid(same_place) = &refusals[1] else {
            unreachable!()
        };
        assert!(
            same_place.contains("a flight needs two different places"),
            "the client refuses in the same words the model's tools do, got: {same_place}",
        );

        let plans = list(&core, account_id).await.unwrap();
        assert!(
            plans[0].trip.items.is_empty(),
            "nothing reached the store"
        );
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
            .upsert_trip(
                account_id,
                "October",
                Some(2),
                Some("economy"),
                Some(conversation_id),
            )
            .unwrap();
        // This test is about the chat a trip names, not about keeping —
        // kept explicitly so the `list` read below can see it.
        store.keep_trip(account_id, "October").unwrap();

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
        seed_trip_for_tests(&core, account_id, "October")
            .await
            .unwrap();

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
            .upsert_trip(
                account_id,
                "October",
                Some(2),
                Some("economy"),
                Some(conversation_id),
            )
            .unwrap();
        seed_trip_for_tests(&core, account_id, "Orphaned")
            .await
            .unwrap();
        // This test is about chat serialization, not about keeping — kept
        // explicitly so the `list` read below can see it.
        store.keep_trip(account_id, "October").unwrap();

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
