# One Timeline for a Trip — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace flight-only `trip_segments` with `trip_items`: flights, stays, activities and transport in one date-ordered list, positions recomputed on every write, flights keeping their candidates and finalising.

**Architecture:** A new `trip_items` + `item_candidates` pair with flights migrated in (steps 14 and 15). The Rust `TripSegment` becomes `TripItem` with `kind` and optional route/place fields; flight-only helpers filter on kind. The store's segment functions become item functions on the new tables, every write ending in `reorder_items`. The flight desk's tools keep their names; a new `add_trip_item` adds non-flights from chat. The page renders one column of cards by kind.

**Tech Stack:** Rust (DuckDB via `duckdb`, rig 0.40, axum 0.8, serde), vanilla JS tested with `node --test`.

**Spec:** `docs/superpowers/specs/2026-09-15-trip-timeline-design.md`. One deviation, decided while planning: a flight's `starts_at` column stays null; the sort time and the view's `starts_at` come from the chosen candidate's `departing_at_local` at read time, so nothing has to keep two columns in step.

**Repo rules:** branch in the main checkout. Do NOT run `cargo fmt`. Watch each new test fail first. Commits end with `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`. Comments say why. The compiler is the checklist for renames: after each rename, `cargo build --workspace` until clean, then the tests.

---

## File structure

| File | Responsibility |
|---|---|
| `crates/scout-core/src/store.rs` | `TripItem`, `NewItem`, `ExpectedItem`; steps 14/15; `load_trip`; item functions; `reorder_items` |
| `crates/scout-core/src/tools/trips.rs` | Flight helpers filter on kind; `FinalisedTrip.fixed_costs`; option tools refuse non-flights; `AddTripItemTool` |
| `crates/scout-core/src/flights.rs` | Registers `add_trip_item`; prompt and guidance lines |
| `crates/scout-core/src/describe.rs` | Progress line for `add_trip_item` |
| `crates/scout-core/src/trips.rs` | `add_leg` without position; `remove_item` with `ExpectedItem` |
| `crates/scout-web/src/routes/chat.rs` | Remove-item body gains `title`; add-leg ignores `position` |
| `crates/scout-web/src/chat.js`, `chat.html`, `chat.test.mjs` | `trip.items`; cards by kind; timeline from flights |
| `README.md`, `docs/BOARD.md` | Docs |

---

### Task 0: Branch

- [ ] `cd /Users/watchcat/work/rust/scout && git checkout main && git pull --ff-only && git checkout -b feat/trip-timeline`

---

### Task 1: The store — types, migration, item functions

**Files:** `crates/scout-core/src/store.rs`

This task is the whole store change in one commit, because the segment functions and the tables they read cannot be swapped one at a time without a half-migrated state. Order inside the task: types → migration → `load_trip` → `reorder_items` → each function → tests green.

- [ ] **Step 1: Failing tests.** Add to `store.rs` `mod tests` (the file's temp-store helper is `test_store() -> (Store, TempDir)`):

```rust
    #[test]
    fn a_version_13_trip_becomes_flight_items_with_its_options_in_date_order() {
        // Two legs stored out of date order (the return first), one chosen
        // option and one undecided pair. After the migration: two flight
        // items, positions by date, every option re-keyed to its item.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scout.duckdb");
        {
            let conn = duckdb::Connection::open(&path).unwrap();
            // The fresh schema still creates the old tables (they are dropped
            // in a later release), so the fixture is: everything, minus the
            // two new tables, recorded as version 13.
            conn.execute_batch(MIGRATIONS).unwrap();
            conn.execute_batch("DROP TABLE IF EXISTS item_candidates; DROP TABLE IF EXISTS trip_items;").unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_version (version BIGINT NOT NULL);
                 DELETE FROM schema_version; INSERT INTO schema_version VALUES (13);
                 INSERT INTO trips (id, account_id, name, name_key, kept) VALUES (1, 1, 'Lisbon', 'lisbon', true);
                 INSERT INTO trip_segments VALUES (1, 1, 'LIS', 'AMS', '2026-10-19', 3), (1, 2, 'AMS', 'LIS', '2026-10-12', 2);
                 INSERT INTO segment_candidates (trip_id, position, candidate, chosen, airline, flight_numbers, itinerary, departing_at_local)
                 VALUES (1, 1, 1, false, 'TAP', 'TP670', 'LIS 18:40 19.10 ✈ AMS 22:55 19.10', '2026-10-19T18:40:00'),
                        (1, 1, 2, false, 'KLM', 'KL1696', 'LIS 06:00 19.10 ✈ AMS 10:15 19.10', '2026-10-19T06:00:00'),
                        (1, 2, 1, true,  'TAP', 'TP671', 'AMS 07:15 12.10 ✈ LIS 09:30 12.10', '2026-10-12T07:15:00');",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), 15);
        let trip = store.find_trip(1, "Lisbon").unwrap().unwrap();
        assert_eq!(trip.items.len(), 2);
        assert_eq!((trip.items[0].position, trip.items[0].date.as_str(), trip.items[0].kind.as_str()), (1, "2026-10-12", "flight"));
        assert_eq!(trip.items[0].origin.as_deref(), Some("AMS"));
        assert_eq!(trip.items[0].title, "AMS → LIS");
        assert_eq!(trip.items[0].candidates.len(), 1);
        assert!(trip.items[0].candidates[0].chosen);
        assert_eq!(trip.items[0].starts_at.as_deref(), Some("2026-10-12T07:15:00"), "a flight's start is its chosen option's departure");
        assert_eq!((trip.items[1].position, trip.items[1].date.as_str()), (2, "2026-10-19"));
        assert_eq!(trip.items[1].candidates.len(), 2);
        assert!(trip.items[1].starts_at.is_none(), "undecided, so no start yet");
        let old: i64 = store.conn().query_row("SELECT count(*) FROM trip_segments", [], |r| r.get(0)).unwrap();
        assert_eq!(old, 2, "the old tables are left in place until a later release drops them");
    }

    fn stay(title: &str, date: &str, ends: &str) -> NewItem {
        NewItem {
            kind: "stay".into(), title: title.into(), place: Some("Lisbon".into()),
            date: date.into(), starts_at: None, ends_at: Some(ends.into()), notes: None,
            booked: false, confirmation_code: None, price: None, currency: None, arrival_id: None,
        }
    }

    #[test]
    fn items_of_every_kind_sort_by_date_then_time_then_kind() {
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Lisbon", None, None, None).unwrap();
        store.add_flight(trip.id, "AMS", "LIS", "2026-10-12").unwrap();
        store.add_item(trip.id, stay("Hotel Alfama", "2026-10-12", "2026-10-15")).unwrap();
        store.add_item(trip.id, NewItem {
            kind: "activity".into(), title: "Azulejo museum".into(), place: Some("Lisbon".into()),
            date: "2026-10-13".into(), starts_at: Some("2026-10-13T10:00:00".into()), ends_at: None,
            notes: None, booked: true, confirmation_code: Some("GYG-1".into()), price: Some(12.0),
            currency: Some("EUR".into()), arrival_id: None,
        }).unwrap();
        let trip = store.add_flight(trip.id, "LIS", "AMS", "2026-10-19").unwrap();
        let order: Vec<(i64, &str, &str)> = trip.items.iter().map(|i| (i.position, i.kind.as_str(), i.title.as_str())).collect();
        // Same date: a flight (no time yet) sorts before a stay by kind rank.
        assert_eq!(order, vec![
            (1, "flight", "AMS → LIS"), (2, "stay", "Hotel Alfama"),
            (3, "activity", "Azulejo museum"), (4, "flight", "LIS → AMS"),
        ]);
        assert!(trip.items[2].booked);
        assert_eq!(trip.items[2].confirmation_code.as_deref(), Some("GYG-1"));
    }

    #[test]
    fn a_time_puts_an_item_before_an_untimed_one_on_the_same_day_and_a_chosen_flight_gets_a_time() {
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Lisbon", None, None, None).unwrap();
        store.add_item(trip.id, stay("Hotel", "2026-10-12", "2026-10-15")).unwrap();
        let trip = store.add_flight(trip.id, "AMS", "LIS", "2026-10-12").unwrap();
        // Flight first by kind rank while it has no time.
        assert_eq!(trip.items[0].kind, "flight");
        let flight_position = trip.items[0].position;
        let trip = store.add_candidate(trip.id, flight_position, expected("AMS", "LIS", Some("2026-10-12")),
            candidate("TAP", "TP671", "2026-10-12T22:00:00"), true).unwrap();
        // The chosen departure is 22:00; the stay has no time; a time still
        // sorts before no time, so the flight stays first.
        assert_eq!(trip.items[0].kind, "flight");
        assert_eq!(trip.items[0].starts_at.as_deref(), Some("2026-10-12T22:00:00"));
    }

    #[test]
    fn dropping_and_re_dating_renumber_by_date() {
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Lisbon", None, None, None).unwrap();
        store.add_flight(trip.id, "AMS", "LIS", "2026-10-12").unwrap();
        store.add_item(trip.id, stay("Hotel", "2026-10-12", "2026-10-15")).unwrap();
        let trip = store.add_flight(trip.id, "LIS", "AMS", "2026-10-19").unwrap();
        let (trip, _, _) = store.update_flight(trip.id, 1, None, None, Some("2026-10-20")).unwrap();
        assert_eq!(trip.items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(), vec!["Hotel", "LIS → AMS", "AMS → LIS"]);
        let trip = store.drop_item(trip.id, 2).unwrap();
        assert_eq!(trip.items.iter().map(|i| (i.position, i.title.as_str())).collect::<Vec<_>>(), vec![(1, "Hotel"), (2, "AMS → LIS")]);
    }

    #[test]
    fn options_go_on_flights_only() {
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Lisbon", None, None, None).unwrap();
        let trip = store.add_item(trip.id, stay("Hotel", "2026-10-12", "2026-10-15")).unwrap();
        let err = store.add_candidate(trip.id, 1, expected("AMS", "LIS", None), candidate("TAP", "TP1", "2026-10-12T07:00:00"), false).unwrap_err();
        assert!(err.to_string().contains("is a stay"), "got: {err}");
    }

    #[test]
    fn removing_an_item_checks_what_the_caller_saw() {
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Lisbon", None, None, None).unwrap();
        store.add_item(trip.id, stay("Hotel", "2026-10-12", "2026-10-15")).unwrap();
        let wrong = ExpectedItem { origin: None, destination: None, title: Some("Hostel"), date: Some("2026-10-12") };
        assert!(!store.remove_item_checked(trip.id, 1, wrong).unwrap());
        let right = ExpectedItem { origin: None, destination: None, title: Some("Hotel"), date: Some("2026-10-12") };
        assert!(store.remove_item_checked(trip.id, 1, right).unwrap());
    }
```

`expected(..)` and `candidate(..)` are whatever helpers the existing candidate tests use to build an `ExpectedSegment`/`NewCandidate`; rename `expected` to build an `ExpectedItem` with `origin`/`destination` as `Some` and `title: None`. If no such helpers exist, add them next to these tests.

- [ ] **Step 2:** `cargo test -p scout-core store::items_of_every` — compile error.

- [ ] **Step 3: Types.** Replace `TripSegment` with:

```rust
/// One thing on a trip: a flight leg, a stay, an activity or a transport
/// booking. Flights carry a route and candidates; the rest carry a title
/// and a place. `position` is 1-based and recomputed by date on every
/// write, so it is a name for talking about the item, not an identity.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TripItem {
    #[serde(skip)]
    pub id: i64,
    pub position: i64,
    /// `flight` | `stay` | `activity` | `transport`
    pub kind: String,
    /// "AMS → LIS" for a flight, the booking's name for the rest.
    pub title: String,
    pub place: Option<String>,
    pub origin: Option<String>,
    pub destination: Option<String>,
    /// The day it starts, `YYYY-MM-DD`; the sort key.
    pub date: String,
    /// Local ISO datetime. For a flight, the chosen option's departure,
    /// filled at read time; `None` while undecided.
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub booked: bool,
    pub confirmation_code: Option<String>,
    pub price: Option<f64>,
    pub currency: Option<String>,
    pub notes: Option<String>,
    pub arrival_id: Option<i64>,
    pub candidates: Vec<TripCandidate>,
}

impl TripItem {
    pub fn is_flight(&self) -> bool {
        self.kind == "flight"
    }
    /// "AMS→LIS" for a flight, the title otherwise: what a sentence about
    /// this item calls it.
    pub fn route(&self) -> String {
        match (&self.origin, &self.destination) {
            (Some(o), Some(d)) => format!("{o}→{d}"),
            _ => self.title.clone(),
        }
    }
}

/// An item on its way into the database. Flights do not come through this;
/// `add_flight` builds theirs.
#[derive(Debug, Clone, PartialEq)]
pub struct NewItem {
    pub kind: String,
    pub title: String,
    pub place: Option<String>,
    pub date: String,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub notes: Option<String>,
    pub booked: bool,
    pub confirmation_code: Option<String>,
    pub price: Option<f64>,
    pub currency: Option<String>,
    pub arrival_id: Option<i64>,
}

/// What the caller saw when it decided to remove an item. Every `Some` must
/// match; `None` is "nothing to verify", not "verified".
pub struct ExpectedItem<'a> {
    pub origin: Option<&'a str>,
    pub destination: Option<&'a str>,
    pub title: Option<&'a str>,
    pub date: Option<&'a str>,
}
```

`Trip.segments: Vec<TripSegment>` becomes `pub items: Vec<TripItem>`, with `impl Trip { pub fn flights(&self) -> impl Iterator<Item = &TripItem> { self.items.iter().filter(|i| i.is_flight()) } }`. `ExpectedSegment` is deleted; `add_candidate` takes `ExpectedItem` and checks `origin`/`destination`/`date` exactly as it checked `ExpectedSegment` (all three `Some` from the flight tools).

- [ ] **Step 4: Migration.** In `MIGRATIONS`, after `segment_candidates`, add the two new tables exactly as the spec's step-14 DDL creates them (`CREATE TABLE IF NOT EXISTS trip_items (...)`, `item_candidates (...)`). Then:

```rust
/// Items replace segments: one table for flights, stays, activities and
/// transport. Flights are copied in with their candidates re-keyed to the
/// new item ids. The old tables stay until a later release drops them, so
/// a rollback of this release still has its data.
const STEP_14_TRIP_ITEMS: &str = r#"
CREATE SEQUENCE IF NOT EXISTS trip_items_id_seq;
CREATE TABLE IF NOT EXISTS trip_items (
    id BIGINT PRIMARY KEY DEFAULT nextval('trip_items_id_seq'),
    trip_id BIGINT NOT NULL, position BIGINT NOT NULL, kind TEXT NOT NULL,
    title TEXT NOT NULL, place TEXT, origin TEXT, destination TEXT,
    starts_at TEXT, ends_at TEXT, date TEXT NOT NULL,
    booked BOOLEAN NOT NULL DEFAULT false, confirmation_code TEXT,
    price DOUBLE, currency TEXT, notes TEXT, arrival_id BIGINT,
    next_candidate BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE TABLE IF NOT EXISTS item_candidates (
    item_id BIGINT NOT NULL, candidate BIGINT NOT NULL,
    chosen BOOLEAN NOT NULL DEFAULT false, airline TEXT NOT NULL, flight_numbers TEXT NOT NULL,
    itinerary TEXT NOT NULL, departing_at_local TEXT, arriving_at_local TEXT,
    duration_minutes BIGINT, quoted_price DOUBLE, quoted_currency TEXT, quoted_at TIMESTAMP,
    source TEXT, PRIMARY KEY (item_id, candidate)
);
INSERT INTO trip_items (trip_id, position, kind, title, origin, destination, date, next_candidate)
SELECT trip_id, position, 'flight', origin || ' → ' || destination, origin, destination, departure_date, next_candidate
FROM trip_segments
WHERE NOT EXISTS (SELECT 1 FROM trip_items i WHERE i.trip_id = trip_segments.trip_id AND i.kind = 'flight');
INSERT INTO item_candidates
SELECT i.id, c.candidate, c.chosen, c.airline, c.flight_numbers, c.itinerary,
       c.departing_at_local, c.arriving_at_local, c.duration_minutes,
       c.quoted_price, c.quoted_currency, c.quoted_at, c.source
FROM segment_candidates c
JOIN trip_items i ON i.trip_id = c.trip_id AND i.position = c.position AND i.kind = 'flight'
WHERE NOT EXISTS (SELECT 1 FROM item_candidates x WHERE x.item_id = i.id AND x.candidate = c.candidate);
"#;

/// Positions were copied as they stood; the date rule puts them right.
fn step_15_reorder_items(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("SELECT DISTINCT trip_id FROM trip_items")?;
    let ids: Vec<i64> = stmt.query_map([], |r| r.get(0))?.collect::<duckdb::Result<_>>()?;
    drop(stmt);
    for id in ids {
        reorder_items(conn, id)?;
    }
    Ok(())
}
```

Register `(14, Step::Sql(STEP_14_TRIP_ITEMS)), (15, Step::Code(step_15_reorder_items))`. The `WHERE NOT EXISTS` guards make the step re-runnable on a database that already carries the tables (the fixture pattern earlier steps document). Bump the `schema_version() == 13` asserts to 15.

- [ ] **Step 5: `reorder_items` and `load_trip`.**

```rust
/// Recomputes positions for one trip: by date, then a start time (an item
/// with one sorts before an item without on the same day; a flight's is its
/// chosen option's departure), then kind — flight, transport, stay,
/// activity — then the previous position, so two items nothing else
/// separates keep their order. Called inside every write, under the lock
/// the caller already holds.
fn reorder_items(conn: &Connection, trip_id: i64) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT i.id FROM trip_items i
         LEFT JOIN item_candidates c ON c.item_id = i.id AND c.chosen
         WHERE i.trip_id = ?
         ORDER BY i.date,
                  COALESCE(i.starts_at, c.departing_at_local) IS NULL,
                  COALESCE(i.starts_at, c.departing_at_local),
                  CASE i.kind WHEN 'flight' THEN 0 WHEN 'transport' THEN 1 WHEN 'stay' THEN 2 ELSE 3 END,
                  i.position, i.id",
    )?;
    let ids: Vec<i64> = stmt.query_map(params![trip_id], |r| r.get(0))?.collect::<duckdb::Result<_>>()?;
    drop(stmt);
    for (n, id) in ids.iter().enumerate() {
        conn.execute("UPDATE trip_items SET position = ? WHERE id = ?", params![n as i64 + 1, id])?;
    }
    Ok(())
}
```

`load_trip` selects from `trip_items` ordered by position (`id, position, kind, title, place, origin, destination, date, starts_at, ends_at, booked, confirmation_code, price, currency, notes, arrival_id`) and candidates from `item_candidates` joined on the trip's item ids (`WHERE item_id IN (SELECT id FROM trip_items WHERE trip_id = ?) ORDER BY item_id, candidate`); a flight's `starts_at`, when the column is null, is the chosen candidate's `departing_at_local`.

- [ ] **Step 6: The functions.** A private `fn item_at(conn, trip_id, position) -> Result<Option<(i64, String)>>` returns `(item_id, kind)`. Then:

  - `add_flight(trip_id, origin, destination, date) -> Result<Trip>`: the `no such trip` check as today; `INSERT INTO trip_items (trip_id, position, kind, title, origin, destination, date) VALUES (?, 0, 'flight', ?, ?, ?, ?)` with title `format!("{origin} → {destination}")`; `reorder_items`; `touch`; `load_trip`. `add_segment_within`, `Inserted` and the position argument go; `add_segment_checked` becomes `add_flight_checked` returning `Option<Trip>` (`None` only for no such trip).
  - `add_item(trip_id, NewItem) -> Result<Trip>`: refuses `kind == "flight"` ("flights go through add_flight"); inserts every field; reorder; touch; load.
  - `update_flight(trip_id, position, origin, destination, date) -> Result<(Trip, usize, bool)>`: `item_at`; refuses a non-flight ("segment {position} is a {kind}, not a flight"); the same no-op and candidate-drop logic as `update_segment` against `trip_items`/`item_candidates` by item id; updates `title` with the route; reorder; touch.
  - `drop_item(trip_id, position) -> Result<Trip>` (`drop_segment_within` generalised): delete the item and its candidates by id; reorder; touch.
  - `remove_item_checked(trip_id, position, ExpectedItem) -> Result<bool>`: reads `origin, destination, title, date` of the item at the position; every `Some` in the expectation must equal; then the drop.
  - `add_candidate(trip_id, position, expected: ExpectedItem, new, decided)`: `item_at`; a non-flight bails "segment {position} is a {kind}; options go on flights"; route/date checks as today against the item's columns; `next_candidate` read and bumped on `trip_items`; insert into `item_candidates (item_id, ...)`; `choose_within(conn, item_id, next)` when decided; reorder (a chosen departure can move it); touch.
  - `choose_within(conn, item_id, candidate)`; `choose_candidate`, `choose_candidate_for_account`, `drop_candidate` resolve the item first and call reorder after.
  - `delete_trip` and `delete_drafts_within` delete `item_candidates` (by item ids of the trip) then `trip_items` then `trips`; the old tables are no longer touched.

- [ ] **Step 7:** `cargo test -p scout-core store::` — all green, existing trip tests adapted where they name `segments`, `departure_date` or an explicit insert position (an explicit position now means nothing; a test asserting "insert at 2" asserts the date order instead).

- [ ] **Step 8: Commit** `feat(store): one timeline of items per trip, flights migrated in`.

---

### Task 2: Flight helpers, the tools, finalising, and `add_trip_item`

**Files:** `crates/scout-core/src/tools/trips.rs`, `flights.rs`, `describe.rs`

- [ ] **Step 1: Failing tests** in `tools/trips.rs` `mod tests` (the file's helpers for building a store and a trip apply):

```rust
    #[test]
    fn connection_notes_skip_the_stay_between_two_flights() {
        // A hotel between landing and the next departure is not a gap in
        // the flying; the check runs on consecutive flights.
        let items = vec![
            flight_item(1, "AMS", "LIS", "2026-10-12", Some(chosen_arriving("2026-10-12T09:30:00"))),
            stay_item(2, "Hotel", "2026-10-12"),
            flight_item(3, "LIS", "AMS", "2026-10-19", Some(chosen_departing("2026-10-19T18:40:00"))),
        ];
        assert!(itinerary_notes(&items).is_empty(), "{:?}", itinerary_notes(&items));
    }

    #[test]
    fn readiness_looks_at_flights_only() {
        let items = vec![
            flight_item(1, "AMS", "LIS", "2026-10-12", Some(chosen_departing("2026-10-12T07:15:00"))),
            stay_item(2, "Hotel", "2026-10-12"),
        ];
        assert_eq!(ready_to_price(&items).unwrap().len(), 1);
        let only_a_stay = vec![stay_item(1, "Hotel", "2026-10-12")];
        assert!(ready_to_price(&only_a_stay).unwrap_err().contains("no flights"));
    }

    #[test]
    fn a_priced_stay_is_a_fixed_cost_and_an_unpriced_one_is_listed() {
        let items = vec![
            stay_item_priced(1, "Hotel", "2026-10-12", Some((320.0, "EUR"))),
            stay_item_priced(2, "Museum", "2026-10-13", None),
        ];
        let (fixed, unpriced) = fixed_costs(&items);
        assert_eq!(fixed, vec![FixedCost { position: 1, title: "Hotel".into(), price: 320.0, currency: "EUR".into() }]);
        assert_eq!(unpriced, vec!["Museum".to_string()]);
    }

    #[tokio::test]
    async fn add_trip_item_puts_a_stay_on_the_trip_and_refuses_a_flight() {
        let (store, account_id, _dir) = tool_store();   // the file's helper; adapt the name
        let tool = AddTripItemTool { store: store.clone(), account_id, conversation_id: 1 };
        let view = tool.call(AddItemArgs {
            trip: "Lisbon".into(), kind: "stay".into(), title: "Hotel Alfama".into(),
            place: Some("Lisbon".into()), date: "2026-10-12".into(), time: None,
            end_date: Some("2026-10-15".into()), notes: None, adults: None, cabin_class: None,
        }).await.unwrap();
        assert_eq!(view.trip.items[0].kind, "stay");
        assert_eq!(view.trip.items[0].ends_at.as_deref(), Some("2026-10-15"));
        assert!(!view.trip.items[0].booked, "the model does not invent a confirmation");
        let err = tool.call(AddItemArgs {
            trip: "Lisbon".into(), kind: "flight".into(), title: "AMS → LIS".into(), place: None,
            date: "2026-10-12".into(), time: None, end_date: None, notes: None, adults: None, cabin_class: None,
        }).await.unwrap_err();
        assert!(err.to_string().contains("add_trip_segment"), "got: {err}");
    }

    #[tokio::test]
    async fn options_are_refused_on_a_stay_with_a_sentence() {
        let (store, account_id, _dir) = tool_store();
        let trip = store.upsert_trip(account_id, "Lisbon", None, None, None).unwrap();
        store.add_item(trip.id, NewItem { kind: "stay".into(), title: "Hotel".into(), place: None, date: "2026-10-12".into(),
            starts_at: None, ends_at: None, notes: None, booked: false, confirmation_code: None, price: None, currency: None, arrival_id: None }).unwrap();
        let tool = ChooseTripOptionTool { store: store.clone(), account_id };
        let err = tool.call(ChooseOptionArgs { trip: "Lisbon".into(), position: 1, candidate: 1 }).await.unwrap_err();
        assert!(err.to_string().contains("is a stay"), "got: {err}");
    }
```

Build the item fixtures (`flight_item`, `stay_item`, `stay_item_priced`, `chosen_departing`, `chosen_arriving`) next to the tests as plain constructors of `TripItem`/`TripCandidate`.

- [ ] **Step 2:** `cargo test -p scout-core tools::trips::` — compile errors.

- [ ] **Step 3: Helpers.** In `tools/trips.rs`:
  - `itinerary_notes(items: &[TripItem])`: collect `items.iter().filter(|i| i.is_flight())` into a `Vec<&TripItem>` and run the existing pair logic over its `windows(2)`, using `route()` and `origin/destination.as_deref().unwrap_or("")` where the strings were.
  - `dates_run_forwards(items)`: unchanged logic over all items, comparing `date`.
  - `ready_to_price(items) -> Result<Vec<(&TripItem, &TripCandidate)>, String>`: flights only; an empty flight list: `Err("this trip has no flights yet, so there is nothing to price")`.
  - New:
    ```rust
    /// A non-flight item with a price, for the finalised total.
    #[derive(Debug, PartialEq, serde::Serialize)]
    pub struct FixedCost { pub position: i64, pub title: String, pub price: f64, pub currency: String }

    /// Priced non-flights, and the titles of the ones with no price.
    pub fn fixed_costs(items: &[TripItem]) -> (Vec<FixedCost>, Vec<String>) {
        let mut fixed = Vec::new();
        let mut unpriced = Vec::new();
        for i in items.iter().filter(|i| !i.is_flight()) {
            match (i.price, &i.currency) {
                (Some(price), Some(currency)) => fixed.push(FixedCost { position: i.position, title: i.title.clone(), price, currency: currency.clone() }),
                _ => unpriced.push(i.title.clone()),
            }
        }
        (fixed, unpriced)
    }
    ```
  - `FinalisedTrip` gains `pub fixed_costs: Vec<FixedCost>` and `pub unpriced_items: Vec<String>`, filled by `finalise_trip` from `fixed_costs(&trip.items)`; a note is added when `unpriced_items` is non-empty: "{n} items on this trip carry no price and are not in the totals". `PricedSegment` keeps its fields, built from `item.route()`-style values (`origin`/`destination` unwrapped: they are flights).

- [ ] **Step 4: Tools.**
  - `AddSegmentArgs` loses `position`; `AddTripSegmentTool` calls `add_flight`.
  - `UpdateTripSegmentTool` calls `update_flight`; `DropTripSegmentTool` calls `drop_item` (or `drop_candidate` when a candidate is given, as today).
  - `AddTripOptionTool` and `ChooseTripOptionTool` surface the store's "is a {kind}" refusal as their error text (they already surface store errors through `StoreToolError`; make sure the sentence is the store's, not rewrapped).
  - New tool:
    ```rust
    #[derive(Debug, Deserialize)]
    pub struct AddItemArgs {
        pub trip: String,
        /// stay | activity | transport. A flight goes through add_trip_segment.
        pub kind: String,
        pub title: String,
        #[serde(default)] pub place: Option<String>,
        /// YYYY-MM-DD, the day it starts.
        pub date: String,
        /// HH:MM local, when known.
        #[serde(default)] pub time: Option<String>,
        /// YYYY-MM-DD for a stay's check-out or a multi-day activity.
        #[serde(default)] pub end_date: Option<String>,
        #[serde(default)] pub notes: Option<String>,
        #[serde(default)] pub adults: Option<i64>,
        #[serde(default)] pub cabin_class: Option<String>,
    }

    pub struct AddTripItemTool { pub store: Store, pub account_id: i64, pub conversation_id: i64 }

    impl Tool for AddTripItemTool {
        const NAME: &'static str = "add_trip_item";
        type Error = StoreToolError;
        type Args = AddItemArgs;
        type Output = TripView;
        fn description(&self) -> String {
            "Puts a stay, an activity or a transport booking on a trip - a hotel, a museum \
             ticket, a train - when the traveller says they have one. Not for flights: \
             add_trip_segment does those. Creates the trip when it does not exist yet, \
             as a draft like add_trip_segment does. Never mark something booked or invent \
             a confirmation code; those come from the traveller's own confirmations."
                .to_string()
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "trip": {"type": "string"},
                    "kind": {"type": "string", "enum": ["stay", "activity", "transport"]},
                    "title": {"type": "string", "description": "the hotel, the ticket, the train"},
                    "place": {"type": "string", "description": "city or address"},
                    "date": {"type": "string", "description": "YYYY-MM-DD, the day it starts"},
                    "time": {"type": "string", "description": "HH:MM local, when known"},
                    "end_date": {"type": "string", "description": "YYYY-MM-DD check-out or last day"},
                    "notes": {"type": "string"},
                    "adults": {"type": "integer"},
                    "cabin_class": {"type": "string"}
                },
                "required": ["trip", "kind", "title", "date"]
            })
        }
        async fn call(&self, a: AddItemArgs) -> Result<TripView, StoreToolError> {
            let kind = a.kind.trim().to_lowercase();
            if !["stay", "activity", "transport"].contains(&kind.as_str()) {
                return Err(StoreToolError(format!(
                    "kind must be stay, activity or transport; a flight goes through add_trip_segment"
                )));
            }
            let date = calendar_date(&a.date)?;
            let starts_at = match &a.time {
                Some(t) => Some(format!("{date}T{}:00", local_time(t)?)),
                None => None,
            };
            let ends_at = a.end_date.as_deref().map(calendar_date).transpose()?;
            // Copy the trip lookup-or-create block from `AddTripSegmentTool::call`
            // verbatim (the lines from its `find_trip` up to, not including, its
            // `add_segment` call): it finds the trip by name for this account
            // or creates it as a draft owned by `conversation_id`, applying
            // `adults`/`cabin_class` when given. Bind the result to `trip`.
            let trip = self.find_or_create(&a.trip, a.adults, a.cabin_class.as_deref()).map_err(StoreToolError)?;
            let before = trip.clone();
            let after = self.store.add_item(trip.id, NewItem {
                kind, title: a.title.trim().to_string(), place: a.place, date, starts_at, ends_at,
                notes: a.notes, booked: false, confirmation_code: None, price: None, currency: None, arrival_id: None,
            }).map_err(StoreToolError)?;
            Ok(view(after, changed_line(&before, "added", &a.title)))
        }
    }
    ```
    Factor that copied block into `fn find_or_create(&self, name, adults, cabin_class) -> anyhow::Result<Trip>` on both tools (or a free function both call) so the two cannot drift. `local_time` validates `HH:MM` (00-23, 00-59) or errors "time must be HH:MM"; `view`/`changed` are the file's existing builders (use their real names). `describe.rs` gets `"add_trip_item" => "🗺️ updating the trip"` (add to the existing arm).
  - `flights.rs`: register `AddTripItemTool` in `build_flight_agent` next to `AddTripSegmentTool`; add `"add_trip_item"` to `TRIP_TOOLS`; in `FLIGHT_PREAMBLE`'s trip rule add: "When the brief says the traveller has a hotel, a ticket or a train, add it with add_trip_item; a trip holds stays and activities as well as flights, and they are shown, not searched." In `TRIP_GUIDANCE` add: "A trip may hold stays, activities and transport beside its flights; present them in the order given, and say when an item is booked and its confirmation code. finalise_trip's fixed_costs are those items' prices as recorded; add them to the flight totals in words, never silently."

- [ ] **Step 5:** `cargo test -p scout-core` — green (the 54 tool tests adapted to `items`/`date`; `flights.rs` tests that list trip tools include the new one).

- [ ] **Step 6: Commit** `feat(trips): stays, activities and transport on a trip, and flights that know they are flights`.

---

### Task 3: Core trip API and web routes

**Files:** `crates/scout-core/src/trips.rs`, `crates/scout-web/src/routes/chat.rs`

- [ ] **Step 1: Failing tests.** In `trips.rs` tests: `remove_item` with a title-only expectation removes a stay and refuses the wrong title; `add_leg` no longer takes a position (update its callers and tests). In `routes/chat.rs` tests: the remove route accepts `{"trip","position","title","date"}` for a stay seeded through a new `seed_item_for_tests` in `scout_core::trips` (`#[doc(hidden)]`, like `seed_trip_for_tests`), and still accepts the flight body; the add-leg route ignores a `position` field.

- [ ] **Step 2:** run — compile errors.

- [ ] **Step 3:** `trips::add_leg(core, account_id, trip_name, origin, destination, departure_date)`; `trips::remove_leg` → `trips::remove_item(core, account_id, trip_name, position, expected: RemoveExpectation { origin: Option<String>, destination: Option<String>, title: Option<String>, date: Option<String> })` building an `ExpectedItem`; when `origin`/`destination` are present they go through `leg_ends` as today. `Plan::from_trip` uses `trip.items`. The web remove handler's body struct gains `title: Option<String>` and passes everything through; the add handler drops `position` from its call (the field may stay on the body struct with `#[serde(default)]` and a comment that it is ignored since positions follow dates).

- [ ] **Step 4:** `cargo test -p scout-core trips::` and `cargo test -p scout-web` — green.

- [ ] **Step 5: Commit** `feat(web): an item of any kind can be removed, and a leg goes where its date puts it`.

---

### Task 4: The page

**Files:** `crates/scout-web/src/chat.js`, `chat.html`, `chat.test.mjs`

- [ ] **Step 1: Failing JS tests** (imports: `tripTimelinePoints`, `removeItemBody`, `itemDateLabel`, `tripRoute`):

```js
test('the timeline is drawn from flights and skips the stays between them', () => {
  const trip = { items: [
    { position: 1, kind: 'flight', origin: 'AMS', destination: 'LIS', date: '2026-10-12', candidates: [] },
    { position: 2, kind: 'stay', title: 'Hotel', date: '2026-10-12', candidates: [] },
    { position: 3, kind: 'flight', origin: 'LIS', destination: 'AMS', date: '2026-10-19', candidates: [] },
  ] }
  assert.deepEqual(tripTimelinePoints(trip).map(p => p.code), ['AMS', 'LIS', 'AMS'])
  assert.equal(tripRoute(trip), 'AMS → LIS → AMS')
})

test('a remove body names what the reader saw, by route for a flight and by title otherwise', () => {
  const flight = JSON.parse(removeItemBody('Lisbon', { position: 1, kind: 'flight', origin: 'AMS', destination: 'LIS', date: '2026-10-12' }))
  assert.deepEqual(flight, { trip: 'Lisbon', position: 1, origin: 'AMS', destination: 'LIS', title: null, date: '2026-10-12' })
  const stay = JSON.parse(removeItemBody('Lisbon', { position: 2, kind: 'stay', title: 'Hotel', date: '2026-10-12' }))
  assert.deepEqual(stay, { trip: 'Lisbon', position: 2, origin: null, destination: null, title: 'Hotel', date: '2026-10-12' })
})

test('an item label shows a range for a stay and a time for a timed activity', () => {
  assert.equal(itemDateLabel({ kind: 'stay', date: '2026-10-12', ends_at: '2026-10-15' }), '12 Oct – 15 Oct')
  assert.equal(itemDateLabel({ kind: 'activity', date: '2026-10-13', starts_at: '2026-10-13T10:00:00' }), '13 Oct, 10:00')
  assert.equal(itemDateLabel({ kind: 'activity', date: '2026-10-13' }), '13 Oct')
})
```

Match `itemDateLabel`'s output to whatever the page's existing `dateLabel` produces for a single date (read it; use its short form), so the two agree.

- [ ] **Step 2:** JS tests fail on the imports.

- [ ] **Step 3:** In `chat.js`: every `trip.segments` → `trip.items`; `segment.departure_date` → `item.date`; `removeLegBody` → `removeItemBody` as above; `tripTimelinePoints` and `tripRoute` iterate `trip.items.filter(i => i.kind === 'flight')`; `connectionCheck` is called on consecutive flights only; `renderSegment` → `renderItem(trip, item)`: flights render the existing card (kicker `Segment N` stays for flights); other kinds render `article.item-card` with kicker `Stay`/`Activity`/`Transport`, `h3` title, a `p.item-place`, a `time` from `itemDateLabel`, a `span.item-booked` "booked · CODE" when `booked`, and the remove button. `renderOverview`'s count line says `n items`. Styles in `chat.html`: `.item-card` shares `.segment-card`'s box; `.item-booked{color:var(--green)}` using the palette's green.

  The PDF export (`tripPdfFilename` and whatever builds the document): include non-flight items as one line each in date order; if that code is a large template, add the line and leave the layout.

- [ ] **Step 4:** `node --test 'crates/scout-web/src/*.test.mjs'`, `cargo test -p scout-web` — green.

- [ ] **Step 5: Commit** `feat(web): the Trips tab shows every kind of item in one column`.

---

### Task 5: Docs

- [ ] README: in the trips section, a sentence that a trip holds stays, activities and transport beside flights, added from chat with "I've booked the Alfama hotel for the 12th to the 15th", ordered by date; the config/diagram lines that say `add_trip_segment` gain `add_trip_item`. Update the test count. `docs/BOARD.md`: move "One timeline for a trip" to In progress (Done with the hash at the end).
- [ ] Commit `docs: one timeline for a trip`.

---

### Task 6: Finish

- [ ] `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && node --test 'crates/scout-web/src/*.test.mjs'`.
- [ ] Merge `--no-ff` as "Merge: one timeline for a trip", push, board Done with the hash (artifact and BOARD.md), deploy with `scripts/deploy-k3s.sh`, confirm `scout is up` and `migration step 15` in the pod log.
- [ ] Live: open the Trips tab; a kept trip from before the deploy shows its legs unchanged and in date order; in chat, "I've booked a hotel in Lisbon 12 to 15 October" puts a stay card on it.
