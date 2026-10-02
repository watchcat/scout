# In-Trip Mode, Slice 2: Moving an Item — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A stay, activity or transport can be moved to another day from its ⋯ menu or by dragging its chip, and Scout then checks whether it works on that day and says so on the card.

**Architecture:** The page posts a move to `/chat/trips/item-move`; the store moves the item under the same stale-tab guard the note and held routes use. A held item is refused until the request says `confirm`. After a move the route starts a background task that runs the main agent in the trip's thread with a brief ending in a `verdict:` line; the verdict is parsed and, when it is a warning, written on the item and sent to the phone. Two nullable columns carry the state: `warning` and `checking_since`.

**Tech Stack:** Rust (`scout-core` store, trips, a new `move_check` module; `scout-web` routes and the printed plan), plain JS (`chat.js`), DuckDB.

**Spec:** `docs/superpowers/specs/2026-10-02-in-trip-mode-design.md`, "Slice 2 — Moving an item".

**Prerequisite:** Slice 1 (`2026-10-02-in-trip-today.md`) is merged. This plan uses its `localToday`, `tripDayRows(trip, locale, today)` and the `queueMicrotask` it added to `renderTripDetail`.

**Repo rules that apply to every task**
- Never run `cargo fmt`.
- The gate, run before any merge:
  - `cargo test --workspace`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `node --test 'crates/scout-web/src/*.test.mjs'`
- Comments say why, not what.
- Every commit message ends with a blank line and
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Work on branch `feat/in-trip-move`: `git checkout -b feat/in-trip-move` before Task 1.
- **Commit before any experiment that edits a tracked file and restores it.** A `git checkout --` on an uncommitted file has cost this repo a day's work once.

## File map

| File | Change |
|---|---|
| `crates/scout-core/src/store.rs` | schema step 20; `TripItem.warning`, `.checking`; `Moved`, `move_item_checked`, `start_item_check`, `finish_item_check`, `dismiss_warning_checked`; `update_item` clears a warning; the daily cap counts two more kinds |
| `crates/scout-core/src/move_check.rs` | new: `Verdict`, `verdict_of`, `brief`, `Move`, `begin`, `check`, `abandon` |
| `crates/scout-core/src/trips.rs` | `MoveEdit`, `move_item`, `dismiss_warning`, `seed_warning_for_tests` |
| `crates/scout-core/src/lib.rs` | `pub mod move_check;` |
| `crates/scout-core/src/flights.rs` | one rule in `FLIGHT_PREAMBLE` |
| `crates/scout-web/src/routes/trips.rs` | `/chat/trips/item-move`, `/chat/trips/item-warning`, `run_check` |
| `crates/scout-web/src/routes/chat.rs` | `reply_to_for`, `queue_conversation` become `pub(crate)` |
| `crates/scout-web/src/trip_pdf.rs` | the warning line |
| `crates/scout-web/src/chat.js`, `chat.html`, `chat.test.mjs` | menu picker, held confirmation, drag, card states, polling |
| `docs/superpowers/specs/2026-10-02-in-trip-mode-design.md` | one sentence: `to` is validated first |

**One change to the spec.** The spec lists the route's checks as: stale tab, flight, `to` not a date, same day, held. This plan validates `to` first, before the store is read: malformed input is refused before anything is looked up, which is how every other route here treats a bad field. Task 5 updates the spec's sentence.

---

### Task 1: Schema step 20 and the two fields

**Files:**
- Modify: `crates/scout-core/src/store.rs`
- Modify (test literals only): `crates/scout-core/src/tools/trips.rs`, `crates/scout-core/src/nearby.rs`, `crates/scout-web/src/trip_pdf.rs`

- [ ] **Step 1: Write the failing test**

In `store.rs`'s `mod tests`, directly after the test `step_19_runs_on_a_database_that_has_items_in_it`, add:

```rust
    /// A database at 19 with a trip item in it. Built from `MIGRATIONS`
    /// and stripped of step 20's columns, for the reason
    /// `version_eighteen_db_with_an_item` gives: a fixture that already has
    /// them makes the step's `IF NOT EXISTS` a no-op and tests nothing.
    fn version_nineteen_db_with_an_item() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("v19.duckdb");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATIONS).unwrap();
        conn.execute_batch(
            "ALTER TABLE trip_items DROP COLUMN warning;
             ALTER TABLE trip_items DROP COLUMN checking_since;
             CREATE TABLE IF NOT EXISTS schema_version (version BIGINT NOT NULL);
             DELETE FROM schema_version;
             INSERT INTO schema_version VALUES (19);
             INSERT INTO accounts (id) VALUES (1);
             INSERT INTO trips (id, account_id, name, name_key) VALUES (1, 1, 'Hong Kong', 'hong kong');
             INSERT INTO trip_items (id, trip_id, position, kind, title, date) VALUES (1, 1, 1, 'activity', 'Lunch', '2026-09-24');",
        )
        .unwrap();
        drop(conn);
        (dir, path)
    }

    #[test]
    fn step_20_runs_on_a_database_that_has_items_in_it_and_matches_a_fresh_one() {
        let (_dir, path) = version_nineteen_db_with_an_item();
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), 20);
        let item = &s.list_trips(1).unwrap()[0].items[0];
        assert_eq!((item.warning.as_deref(), item.checking), (None, false));
        let (fresh, _d) = test_store();
        assert_eq!(shape(&fresh.conn(), "trip_items"), shape(&s.conn(), "trip_items"));
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p scout-core step_20_runs 2>&1 | grep -E "^error" -A4 | head -12`
Expected: a compile error, `no field warning on type TripItem`.

- [ ] **Step 3: Implement the schema**

In `MIGRATIONS`, in the `trip_items` DDL, replace

```sql
    lat               DOUBLE,
    lng               DOUBLE,
    geocode_tried     BOOLEAN DEFAULT false
);
```

with

```sql
    lat               DOUBLE,
    lng               DOUBLE,
    geocode_tried     BOOLEAN DEFAULT false,
    -- What Scout said about this item the last time it was moved, when
    -- that was a warning, and when a check of it began. Step 20. Both
    -- nullable, for the reason `geocode_tried` is: nothing is made NOT
    -- NULL by a migration of a table with rows.
    warning           TEXT,
    checking_since    TIMESTAMP
);
```

Directly above `fn steps() -> Vec<(i64, Step)> {`, add:

```rust
/// A moved item's verdict and whether one is being worked out. Added in
/// place and left nullable; see `STEP_19_ITEM_COORDS` for what happened
/// to the step that tried to constrain a column it had just filled.
const STEP_20_ITEM_CHECK: &str = r#"
ALTER TABLE trip_items ADD COLUMN IF NOT EXISTS warning TEXT;
ALTER TABLE trip_items ADD COLUMN IF NOT EXISTS checking_since TIMESTAMP;
"#;
```

In `steps()`, after `(19, Step::Sql(STEP_19_ITEM_COORDS)),` add `(20, Step::Sql(STEP_20_ITEM_CHECK)),`.

Bump the version every test pins:

```bash
sed -i '' 's/schema_version().unwrap(), 19/schema_version().unwrap(), 20/g' crates/scout-core/src/store.rs
```

- [ ] **Step 4: Implement the fields**

In `pub struct TripItem`, after the field `pub geocode_tried: bool,` add:

```rust
    /// What Scout said about this item when it was last moved, if that was
    /// a warning. Cleared by a move, by an edit of its day, time or place,
    /// and by the reader dismissing it.
    pub warning: Option<String>,
    /// True while a check of this item is running. Read off
    /// `checking_since`, and only for five minutes: a check that died with
    /// the pod must not leave the card saying "Checking…" for good.
    pub checking: bool,
```

In `load_trip`, replace

```rust
                booked, confirmation_code, price, currency, notes, arrival_id, lat, lng, geocode_tried
         FROM trip_items WHERE trip_id = ? ORDER BY position",
```

with

```rust
                booked, confirmation_code, price, currency, notes, arrival_id, lat, lng, geocode_tried,
                warning,
                (checking_since IS NOT NULL
                 AND checking_since > CAST(current_timestamp AS TIMESTAMP) - to_seconds(300))
         FROM trip_items WHERE trip_id = ? ORDER BY position",
```

and replace `                geocode_tried: r.get::<_, Option<bool>>(18)?.unwrap_or(false),` with

```rust
                geocode_tried: r.get::<_, Option<bool>>(18)?.unwrap_or(false),
                warning: r.get(19)?,
                checking: r.get(20)?,
```

- [ ] **Step 5: Fill the test literals**

Every hand-written `TripItem { … }` in the tests now lacks two fields. Find them:

```bash
cargo build --workspace --all-targets 2>&1 | grep -A2 "E0063" | grep -- "-->"
```

Expected sites: two in `crates/scout-core/src/tools/trips.rs`, one in `crates/scout-core/src/nearby.rs` (the `item` helper), three in `crates/scout-web/src/trip_pdf.rs`. In each, directly after the line `geocode_tried: false,` (in `nearby.rs`, after `geocode_tried: coords.is_some(),`), add:

```rust
            warning: None,
            checking: false,
```

keeping the indentation of the line above.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p scout-core store:: 2>&1 | grep -E "FAILED|panicked|test result"`
Expected: `test result: ok`.

- [ ] **Step 7: Commit**

```bash
git add -A crates
git commit -m "feat(store): a moved item's warning and whether it is being checked (schema 20)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: The store moves an item, and keeps its verdict honest

**Files:**
- Modify: `crates/scout-core/src/store.rs`

- [ ] **Step 1: Write the failing tests**

In `store.rs`'s `mod tests`, add:

```rust
    fn moving_trip(s: &Store) -> (i64, i64) {
        let a = s.account_for_telegram(11).unwrap();
        let trip = s.upsert_trip(a, "Hong Kong", None, None, None).unwrap();
        let new = |kind: &str, title: &str, date: &str, starts: Option<&str>, ends: Option<&str>, booked: bool| NewItem {
            kind: kind.into(),
            title: title.into(),
            place: None,
            date: date.into(),
            starts_at: starts.map(Into::into),
            ends_at: ends.map(Into::into),
            notes: None,
            booked,
            confirmation_code: None,
            price: None,
            currency: None,
            arrival_id: None,
        };
        // Flight on the 21st is item 1, the hotel 2, the lunch 3.
        s.add_flight(trip.id, "AMS", "HKG", "2026-09-21").unwrap();
        s.add_item(trip.id, new("stay", "Hotel", "2026-09-22", None, Some("2026-09-25T11:00:00"), true)).unwrap();
        s.add_item(trip.id, new("activity", "Lunch", "2026-09-24", Some("2026-09-24T14:30:00"), None, false)).unwrap();
        (a, trip.id)
    }

    fn seen<'a>(title: &'a str, date: &'a str) -> ExpectedItem<'a> {
        ExpectedItem { origin: None, destination: None, title: Some(title), date: Some(date) }
    }

    #[test]
    fn a_move_keeps_a_clocks_time_and_a_stays_nights_and_asks_before_moving_what_is_held() {
        let (s, _d) = test_store();
        let (a, trip) = moving_trip(&s);
        let item = |title: &str| s.trip_by_id(a, trip).unwrap().unwrap().items.into_iter().find(|i| i.title == title).unwrap();

        // A plan moves at once, and its clock comes with it.
        let moved = s.move_item_checked(trip, 3, seen("Lunch", "2026-09-24"), "2026-09-23", false).unwrap();
        assert!(matches!(&moved, Moved::Done { title, from, .. } if title == "Lunch" && from == "2026-09-24"), "{moved:?}");
        assert_eq!(item("Lunch").date, "2026-09-23");
        assert_eq!(item("Lunch").starts_at.as_deref(), Some("2026-09-23T14:30:00"));

        // A stale tab, the same day, and a flight change nothing.
        assert_eq!(s.move_item_checked(trip, 3, seen("Dinner", "2026-09-23"), "2026-09-26", false).unwrap(), Moved::Stale);
        assert_eq!(s.move_item_checked(trip, 3, seen("Lunch", "2026-09-23"), "2026-09-23", false).unwrap(), Moved::Same);
        let leg = ExpectedItem { origin: Some("AMS"), destination: Some("HKG"), title: None, date: Some("2026-09-21") };
        assert_eq!(s.move_item_checked(trip, 1, leg, "2026-09-22", true).unwrap(), Moved::Flight);

        // A held stay asks first, and then moves as a block: three nights.
        assert_eq!(s.move_item_checked(trip, 2, seen("Hotel", "2026-09-22"), "2026-09-25", false).unwrap(), Moved::NeedsConfirm);
        assert_eq!(item("Hotel").date, "2026-09-22");
        assert!(matches!(s.move_item_checked(trip, 2, seen("Hotel", "2026-09-22"), "2026-09-25", true).unwrap(), Moved::Done { .. }));
        assert_eq!(item("Hotel").date, "2026-09-25");
        assert_eq!(item("Hotel").ends_at.as_deref(), Some("2026-09-28T11:00:00"));
        // And the list is in date order again: the hotel is last now.
        assert_eq!(item("Hotel").position, 3);
    }

    #[test]
    fn a_verdict_is_written_only_on_the_day_it_was_about_and_goes_when_the_item_is_rescheduled() {
        let (s, _d) = test_store();
        let (a, trip) = moving_trip(&s);
        let item = |title: &str| s.trip_by_id(a, trip).unwrap().unwrap().items.into_iter().find(|i| i.title == title).unwrap();
        let lunch = item("Lunch").id;

        s.start_item_check(lunch).unwrap();
        assert!(item("Lunch").checking);
        // About another day: the item moved on while the check ran.
        assert!(!s.finish_item_check(lunch, "2026-09-23", Some("closed on Wednesdays")).unwrap());
        assert_eq!(item("Lunch").warning, None);
        assert!(item("Lunch").checking, "a check of another day is not this day's to end");
        assert!(s.finish_item_check(lunch, "2026-09-24", Some("closed on Thursdays")).unwrap());
        assert_eq!(item("Lunch").warning.as_deref(), Some("closed on Thursdays"));
        assert!(!item("Lunch").checking);
        // A verdict of fine writes nothing and still ends the check.
        s.start_item_check(lunch).unwrap();
        assert!(!s.finish_item_check(lunch, "2026-09-24", None).unwrap());
        assert!(!item("Lunch").checking);

        // A new name leaves the warning; a new time or day takes it.
        s.finish_item_check(lunch, "2026-09-24", Some("closed on Thursdays")).unwrap();
        s.update_item(trip, 3, ItemEdit { title: Some("Late lunch"), ..Default::default() }).unwrap();
        assert!(item("Late lunch").warning.is_some());
        s.update_item(trip, 3, ItemEdit { time: Some("15:00"), ..Default::default() }).unwrap();
        assert_eq!(item("Late lunch").warning, None);
        s.finish_item_check(lunch, "2026-09-24", Some("closed on Thursdays")).unwrap();
        assert!(matches!(s.move_item_checked(trip, 3, seen("Late lunch", "2026-09-24"), "2026-09-26", false).unwrap(), Moved::Done { .. }));
        assert_eq!(item("Late lunch").warning, None);

        // Dismissed by the reader, under the stale-tab guard.
        s.finish_item_check(lunch, "2026-09-26", Some("closed on Saturdays")).unwrap();
        assert!(!s.dismiss_warning_checked(trip, 3, seen("Lunch", "2026-09-26")).unwrap());
        assert!(s.dismiss_warning_checked(trip, 3, seen("Late lunch", "2026-09-26")).unwrap());
        assert_eq!(item("Late lunch").warning, None);
    }

    #[test]
    fn a_move_check_and_a_document_count_toward_the_day() {
        // The cap counted `text` and `photo`. A PDF sent to the bot was
        // logged as `document` and counted for nothing, and a check would
        // have been the same.
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        for kind in ["text", "photo", "document", "move_check", "something else"] {
            s.log_request(a, kind).unwrap();
        }
        assert_eq!(s.requests_today(a).unwrap(), 4);
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p scout-core store::tests::a_move 2>&1 | grep -E "^error" -A3 | head -10`
Expected: `cannot find type Moved`, `no method named move_item_checked`.

- [ ] **Step 3: Implement**

Directly above `pub struct ExpectedItem<'a>`'s doc comment, add:

```rust
/// What became of a move asked from the page.
#[derive(Debug, Clone, PartialEq)]
pub enum Moved {
    /// Not the item the caller drew: reload and look again.
    Stale,
    /// A leg's date is its ticket's, and `update_flight`'s to change.
    Flight,
    /// Already on that day.
    Same,
    /// Held, and the caller has not said to move it anyway. The card
    /// moving does not move the booking, so the question is the server's
    /// to insist on and not only the page's to ask.
    NeedsConfirm,
    Done { item_id: i64, title: String, from: String },
}
```

In `impl Store`, directly above `pub fn hold_item_checked`, add:

```rust
    /// Moves an item to another day, on the item the caller still says it
    /// is looking at — `note_item_checked`'s guard.
    ///
    /// Everything dated on the item moves by the same number of days: a
    /// lunch at 14:30 stays at 14:30, and a stay keeps its nights, because
    /// a check-out left behind would turn three nights into one or into
    /// a stay that ends before it starts. The old verdict goes with the
    /// old day.
    pub fn move_item_checked(
        &self,
        trip_id: i64,
        position: i64,
        expected: ExpectedItem<'_>,
        to: &str,
        confirm: bool,
    ) -> Result<Moved> {
        let conn = self.conn();
        let Some(item_id) = item_still_seen(&conn, trip_id, position, expected)? else {
            return Ok(Moved::Stale);
        };
        let (kind, title, date, starts_at, ends_at, booked) = conn.query_row(
            "SELECT kind, title, date, starts_at, ends_at, booked FROM trip_items WHERE id = ?",
            params![item_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, bool>(5)?,
                ))
            },
        )?;
        if kind == "flight" {
            return Ok(Moved::Flight);
        }
        if date == to {
            return Ok(Moved::Same);
        }
        if booked && !confirm {
            return Ok(Moved::NeedsConfirm);
        }
        // The day a stamp is on is its first ten characters; what follows
        // — a clock, or nothing — is kept as it is.
        let day = |stamp: &str| chrono::NaiveDate::parse_from_str(stamp.get(..10).unwrap_or(stamp), "%Y-%m-%d");
        let shift = day(to)? - day(&date)?;
        let starts = starts_at.map(|at| format!("{to}{}", at.get(10..).unwrap_or("")));
        let ends = match ends_at {
            Some(at) => Some(format!("{}{}", day(&at)? + shift, at.get(10..).unwrap_or(""))),
            None => None,
        };
        conn.execute(
            "UPDATE trip_items SET date = ?, starts_at = ?, ends_at = ?, warning = NULL, checking_since = NULL,
                 updated_at = current_timestamp
             WHERE id = ?",
            params![to, starts, ends, item_id],
        )?;
        reorder_items(&conn, trip_id)?;
        touch(&conn, trip_id)?;
        Ok(Moved::Done { item_id, title, from: date })
    }

    /// A check of this item has begun. The same expression `load_trip`
    /// compares against, so the two cannot disagree by a time zone.
    pub fn start_item_check(&self, item_id: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE trip_items SET checking_since = CAST(current_timestamp AS TIMESTAMP) WHERE id = ?",
            params![item_id],
        )?;
        Ok(())
    }

    /// The check is over: its verdict, if it was a warning, and the end of
    /// "Checking…" either way — but only while the item is still on the
    /// day the check was about. One moved again in the meantime has a
    /// check of its own, and this one has nothing to say to it. `true`
    /// when a warning was written.
    pub fn finish_item_check(&self, item_id: i64, on_date: &str, warning: Option<&str>) -> Result<bool> {
        let conn = self.conn();
        let changed = conn.execute(
            "UPDATE trip_items SET checking_since = NULL, warning = ? WHERE id = ? AND date = ?",
            params![warning, item_id, on_date],
        )?;
        Ok(changed > 0 && warning.is_some())
    }

    /// The reader has read the warning and wants it gone. Guarded like
    /// every write from a card.
    pub fn dismiss_warning_checked(&self, trip_id: i64, position: i64, expected: ExpectedItem<'_>) -> Result<bool> {
        let conn = self.conn();
        let Some(item_id) = item_still_seen(&conn, trip_id, position, expected)? else {
            return Ok(false);
        };
        conn.execute("UPDATE trip_items SET warning = NULL WHERE id = ?", params![item_id])?;
        touch(&conn, trip_id)?;
        Ok(true)
    }
```

In `update_item`, replace

```rust
        let moved = wanted_place != place;
        conn.execute(
            "UPDATE trip_items SET title = ?, place = ?, date = ?, starts_at = ?, ends_at = ?,
                 booked = ?, confirmation_code = ?, updated_at = current_timestamp,
                 lat = CASE WHEN ? THEN NULL ELSE lat END,
                 lng = CASE WHEN ? THEN NULL ELSE lng END,
                 geocode_tried = CASE WHEN ? THEN false ELSE geocode_tried END
             WHERE id = ?",
            params![
                wanted_title,
                wanted_place,
                wanted_date,
                wanted_starts,
                wanted_ends,
                wanted_booked,
                wanted_code,
                moved,
                moved,
                moved,
                item_id
            ],
        )?;
```

with

```rust
        let moved = wanted_place != place;
        // A warning is about the item on a day, at a time, in a place.
        // Change any of those and it is about something that is no longer
        // true; a new name or a new code leaves it standing.
        let rescheduled = moved || wanted_date != date || wanted_starts != starts_at;
        conn.execute(
            "UPDATE trip_items SET title = ?, place = ?, date = ?, starts_at = ?, ends_at = ?,
                 booked = ?, confirmation_code = ?, updated_at = current_timestamp,
                 lat = CASE WHEN ? THEN NULL ELSE lat END,
                 lng = CASE WHEN ? THEN NULL ELSE lng END,
                 geocode_tried = CASE WHEN ? THEN false ELSE geocode_tried END,
                 warning = CASE WHEN ? THEN NULL ELSE warning END
             WHERE id = ?",
            params![
                wanted_title,
                wanted_place,
                wanted_date,
                wanted_starts,
                wanted_ends,
                wanted_booked,
                wanted_code,
                moved,
                moved,
                moved,
                rescheduled,
                item_id
            ],
        )?;
```

In `requests_today`, replace `             WHERE account_id = ? AND kind IN ('text', 'photo')` with

```rust
             WHERE account_id = ? AND kind IN ('text', 'photo', 'document', 'move_check')
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p scout-core store:: 2>&1 | grep -E "FAILED|panicked|test result"`
Expected: `test result: ok`.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/store.rs
git commit -m "feat(store): move an item between days, and keep its verdict about the day it was given for

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: The check — brief, verdict, and the task that runs it

**Files:**
- Create: `crates/scout-core/src/move_check.rs`
- Modify: `crates/scout-core/src/lib.rs`

- [ ] **Step 1: Create the module with its tests and no implementation**

In `crates/scout-core/src/lib.rs`, after `pub mod mirror;` add `pub mod move_check;`.

Create `crates/scout-core/src/move_check.rs`:

```rust
//! Whether a moved item works on its new day.
//!
//! The move has already happened: the page moves an item at once and this
//! is the second opinion that follows it. It is an ordinary run of the
//! main agent in the trip's own thread — the desk cannot search the web,
//! and whether a shop is open on a Wednesday is a web question — so the
//! check is recorded in the chat like anything else Scout was asked.
//!
//! What is here is everything but the run itself: the brief, the reading
//! of the reply, and the rule for what is written down. The run is handed
//! in as a closure, which is what lets the rule be tested without a model.

use crate::core::{blocking, Core};
use crate::run::RunOutcome;
use std::time::Duration;

/// How many times a check asks before it gives up on a thread that is
/// busy with another run, and how long it waits between. Two minutes in
/// all: a run in the same thread is usually the reader's own question.
pub const BUSY_TRIES: usize = 12;
pub const BUSY_PAUSE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Fine,
    Warn(String),
    /// No line that says, or one that cannot be read. Nothing is written;
    /// the reply is in the chat for anyone who looks.
    Unknown,
}

/// One item moved by the traveller, as the check needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct Move {
    pub account_id: i64,
    pub item_id: i64,
    pub trip: String,
    pub title: String,
    pub from: String,
    pub to: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewItem;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn the_verdict_is_the_last_line_and_nothing_else_is_one() {
        assert_eq!(verdict_of("Looks good.\n\nverdict: fine"), Verdict::Fine);
        assert_eq!(verdict_of("Verdict: Fine.\n\n"), Verdict::Fine);
        assert_eq!(verdict_of("**verdict: fine**"), Verdict::Fine);
        assert_eq!(
            verdict_of("The shop is shut.\nverdict: warn - APM is closed on Wednesdays"),
            Verdict::Warn("APM is closed on Wednesdays".into())
        );
        assert_eq!(verdict_of("VERDICT: WARN — You land at 15:25 that day"), Verdict::Warn("You land at 15:25 that day".into()));
        // A verdict that is not the last line is prose about a verdict.
        assert_eq!(verdict_of("verdict: fine\nActually, one more thing."), Verdict::Unknown);
        assert_eq!(verdict_of("verdict: warn"), Verdict::Unknown, "a warning with no reason is not one");
        assert_eq!(verdict_of("verdict: maybe"), Verdict::Unknown);
        assert_eq!(verdict_of("It should be fine."), Verdict::Unknown);
        assert_eq!(verdict_of(""), Verdict::Unknown);
    }

    #[test]
    fn the_brief_reads_as_what_the_traveller_did_and_tells_the_model_the_rest() {
        let text = brief("Lunch with Stanley", "Hong Kong, September", "2026-09-24", "2026-09-23");
        // The first line is what the thread shows; the note is cut from it.
        assert!(text.starts_with("Moved Lunch with Stanley from Thu 24 Sep 2026 to Wed 23 Sep 2026. Check it.\n\n[system note] "), "{text}");
        for must in [
            "on the trip \"Hong Kong, September\"",
            "it is already moved",
            "Ask the desk what the trip holds",
            "two searches at most",
            "Do not change the trip",
            "exactly \"verdict: fine\" or \"verdict: warn - <reason in one sentence>\"",
        ] {
            assert!(text.contains(must), "the brief lost {must:?}: {text}");
        }
    }

    fn core() -> (Core, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("check.duckdb").to_str().unwrap().to_string();
        (Core::start(crate::config::Config::for_test(&p), None).unwrap(), dir)
    }

    /// A lunch on the 23rd, moved there from the 24th, with a check begun.
    async fn moved_lunch(core: &Core) -> Move {
        let store = core.store();
        let account_id = store.account_for_telegram(1).unwrap();
        store.note_delivery(account_id, crate::mirror::TELEGRAM, "4242").unwrap();
        let trip = store.upsert_trip(account_id, "Hong Kong", None, None, None).unwrap();
        let trip = store
            .add_item(trip.id, NewItem {
                kind: "activity".into(),
                title: "Lunch".into(),
                place: None,
                date: "2026-09-23".into(),
                starts_at: None,
                ends_at: None,
                notes: None,
                booked: false,
                confirmation_code: None,
                price: None,
                currency: None,
                arrival_id: None,
            })
            .unwrap();
        let m = Move {
            account_id,
            item_id: trip.items[0].id,
            trip: "Hong Kong".into(),
            title: "Lunch".into(),
            from: "2026-09-24".into(),
            to: "2026-09-23".into(),
        };
        begin(core, m.item_id).await.unwrap();
        m
    }

    fn lunch(core: &Core, m: &Move) -> crate::store::TripItem {
        let trip = core.store().find_trip(m.account_id, "Hong Kong").unwrap().unwrap();
        trip.items.into_iter().find(|i| i.id == m.item_id).unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn a_warning_is_written_on_the_item_and_sent_to_the_phone_once() {
        let (core, _dir) = core();
        let m = moved_lunch(&core).await;
        let answer = || async { Ok::<_, anyhow::Error>(RunOutcome::Answered("Shut.\nverdict: warn - closed on Wednesdays".into())) };
        assert_eq!(check(&core, &m, |_| answer()).await, Verdict::Warn("closed on Wednesdays".into()));
        assert_eq!(lunch(&core, &m).warning.as_deref(), Some("closed on Wednesdays"));
        assert!(!lunch(&core, &m).checking);
        // The same verdict again — the item moved away and back — says it once.
        begin(&core, m.item_id).await.unwrap();
        check(&core, &m, |_| answer()).await;
        let queued = crate::mirror::pending(&core, 10).await.unwrap();
        let lines: Vec<&str> = queued.iter().map(|row| row.body.as_str()).collect();
        assert_eq!(lines, vec!["Lunch on Wed 23 Sep 2026: closed on Wednesdays"]);
    }

    #[tokio::test(start_paused = true)]
    async fn fine_and_unreadable_write_nothing_and_still_end_the_check() {
        let (core, _dir) = core();
        let m = moved_lunch(&core).await;
        assert_eq!(check(&core, &m, |_| async { Ok(RunOutcome::Answered("verdict: fine".into())) }).await, Verdict::Fine);
        assert_eq!((lunch(&core, &m).warning, lunch(&core, &m).checking), (None, false));
        begin(&core, m.item_id).await.unwrap();
        assert_eq!(check(&core, &m, |_| async { Ok(RunOutcome::Answered("Probably fine?".into())) }).await, Verdict::Unknown);
        assert_eq!((lunch(&core, &m).warning, lunch(&core, &m).checking), (None, false));
        begin(&core, m.item_id).await.unwrap();
        assert_eq!(check(&core, &m, |_| async { Err(anyhow::anyhow!("the model is down")) }).await, Verdict::Unknown);
        assert!(!lunch(&core, &m).checking);
        assert!(crate::mirror::pending(&core, 10).await.unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_busy_thread_is_asked_again_and_then_given_up_on() {
        let (core, _dir) = core();
        let m = moved_lunch(&core).await;
        // Busy twice, then an answer.
        let asked = AtomicUsize::new(0);
        let verdict = check(&core, &m, |_| {
            let n = asked.fetch_add(1, Ordering::Relaxed);
            async move {
                Ok(if n < 2 { RunOutcome::Busy } else { RunOutcome::Answered("verdict: fine".into()) })
            }
        })
        .await;
        assert_eq!((verdict, asked.load(Ordering::Relaxed)), (Verdict::Fine, 3));

        // Busy for good: asked `BUSY_TRIES` times, then nothing written
        // and the card stops saying "Checking…".
        begin(&core, m.item_id).await.unwrap();
        let asked = AtomicUsize::new(0);
        let verdict = check(&core, &m, |_| {
            asked.fetch_add(1, Ordering::Relaxed);
            async { Ok(RunOutcome::Overloaded) }
        })
        .await;
        assert_eq!((verdict, asked.load(Ordering::Relaxed)), (Verdict::Unknown, BUSY_TRIES));
        assert!(!lunch(&core, &m).checking);
    }

    #[tokio::test(start_paused = true)]
    async fn a_verdict_about_a_day_the_item_has_left_is_not_written() {
        let (core, _dir) = core();
        let m = moved_lunch(&core).await;
        let store = core.store();
        let trip = store.find_trip(m.account_id, "Hong Kong").unwrap().unwrap();
        let seen = crate::store::ExpectedItem { origin: None, destination: None, title: Some("Lunch"), date: Some("2026-09-23") };
        store.move_item_checked(trip.id, 1, seen, "2026-09-26", false).unwrap();
        let verdict = check(&core, &m, |_| async { Ok(RunOutcome::Answered("verdict: warn - closed on Wednesdays".into())) }).await;
        assert_eq!(verdict, Verdict::Warn("closed on Wednesdays".into()));
        assert_eq!(lunch(&core, &m).warning, None);
        assert!(crate::mirror::pending(&core, 10).await.unwrap().is_empty(), "nothing is sent about a day it is not on");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p scout-core move_check 2>&1 | grep -E "^error" -A3 | head -12`
Expected: `cannot find function verdict_of`, `brief`, `begin`, `check`.

- [ ] **Step 3: Implement**

In `move_check.rs`, between the `Move` struct and `#[cfg(test)]`, add:

```rust
/// `prefix` off the front of `s`, whatever its case. On the original
/// string, not a lowered copy: lowering can change a string's length, and
/// the reason is cut from the model's own words.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[prefix.len()..])
}

/// The verdict a reply ends with. Only the last line counts: a verdict
/// followed by more prose is the model talking about one, not giving it.
pub fn verdict_of(reply: &str) -> Verdict {
    let Some(line) = reply.lines().rev().map(str::trim).find(|line| !line.is_empty()) else {
        return Verdict::Unknown;
    };
    // A model that sets the whole line in bold or code still said it.
    let line = line.trim_matches(|c| matches!(c, '*' | '`' | '_'));
    let Some(rest) = strip_prefix_ci(line, "verdict:") else {
        return Verdict::Unknown;
    };
    let rest = rest.trim();
    if rest.trim_end_matches('.').eq_ignore_ascii_case("fine") {
        return Verdict::Fine;
    }
    let Some(reason) = strip_prefix_ci(rest, "warn") else {
        return Verdict::Unknown;
    };
    let reason = reason.trim_start_matches(|c: char| matches!(c, '-' | '—' | '–' | ':') || c.is_whitespace()).trim();
    if reason.is_empty() {
        Verdict::Unknown
    } else {
        Verdict::Warn(reason.to_string())
    }
}

/// "Wed 23 Sep 2026", with the year: the main agent is told to write the
/// year out whenever a date goes on to a tool or another agent.
fn day_label(date: &str) -> String {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|d| d.format("%a %-d %b %Y").to_string())
        .unwrap_or_else(|_| date.to_string())
}

/// What the run is asked. The first line reads as something the traveller
/// did, because the thread shows it as their turn; the rest is a system
/// note, which the transcript cuts and the model reads.
pub fn brief(title: &str, trip: &str, from: &str, to: &str) -> String {
    format!(
        "Moved {title} from {} to {}. Check it.\n\n\
         [system note] The traveller moved this item on the trip \"{trip}\" themselves; it is already moved. \
         Ask the desk what the trip holds. Say whether the item works on its new day: are they in that city \
         that day, by the flights and the stay; does it clash with anything timed; and if it names a place, \
         search for that place's opening days and hours on that date - two searches at most. \
         Do not change the trip. End with one line, exactly \"verdict: fine\" or \
         \"verdict: warn - <reason in one sentence>\".",
        day_label(from),
        day_label(to),
    )
}

/// Marks the item as being checked, so the card says so from the response
/// to the move rather than five seconds later.
pub async fn begin(core: &Core, item_id: i64) -> anyhow::Result<()> {
    let store = core.store();
    blocking(move || store.start_item_check(item_id)).await
}

/// Ends a check that never ran — no thread to run it in — so the card
/// stops saying it is being checked.
pub async fn abandon(core: &Core, m: &Move) {
    let store = core.store();
    let (item_id, to) = (m.item_id, m.to.clone());
    if let Err(e) = blocking(move || store.finish_item_check(item_id, &to, None)).await {
        tracing::warn!(error = %e, item_id, "could not end a check that never ran");
    }
}

/// Asks, reads the verdict, writes it down, and tells the phone when it is
/// a warning. `ask` is the run: it is handed the brief and answers with
/// what `run_agent` returned.
///
/// Every way out ends the check on the item, which is the one thing this
/// must not forget: a card left saying "Checking…" is a page polling for
/// an answer that is not coming.
pub async fn check<F, Fut>(core: &Core, m: &Move, mut ask: F) -> Verdict
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<RunOutcome>>,
{
    let prompt = brief(&m.title, &m.trip, &m.from, &m.to);
    let mut verdict = Verdict::Unknown;
    for attempt in 0..BUSY_TRIES {
        match ask(prompt.clone()).await {
            Ok(RunOutcome::Answered(reply)) => {
                verdict = verdict_of(&reply);
                break;
            }
            // The thread has a run in it, or every slot is taken. Both
            // pass; neither is a reason to drop the check at once.
            Ok(RunOutcome::Busy | RunOutcome::Overloaded) => {
                if attempt + 1 < BUSY_TRIES {
                    tokio::time::sleep(BUSY_PAUSE).await;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, item_id = m.item_id, "a move could not be checked");
                break;
            }
        }
    }
    let warning = match &verdict {
        Verdict::Warn(reason) => Some(reason.clone()),
        _ => None,
    };
    let store = core.store();
    let (item_id, to, reason) = (m.item_id, m.to.clone(), warning.clone());
    match blocking(move || store.finish_item_check(item_id, &to, reason.as_deref())).await {
        Ok(true) => {
            if let Err(e) = nudge(core, m, warning.as_deref().unwrap_or_default()).await {
                tracing::warn!(error = %e, item_id = m.item_id, "could not send a move's warning to the phone");
            }
        }
        // Fine, unknown, or about a day the item has left.
        Ok(false) => {}
        Err(e) => tracing::warn!(error = %e, item_id = m.item_id, "could not record a move's verdict"),
    }
    verdict
}

/// One line to the phone, the way a booking's arrival is said. Keyed on
/// the item and the day, so the same warning about the same day — the
/// item moved away and back — is said once.
async fn nudge(core: &Core, m: &Move, reason: &str) -> anyhow::Result<bool> {
    let store = core.store();
    let body = format!("{} on {}: {reason}", m.title, day_label(&m.to));
    let key = format!("move:{}:{}", m.item_id, m.to);
    let account_id = m.account_id;
    let queued = blocking(move || {
        let Some(address) = store.delivery_address(account_id, crate::mirror::TELEGRAM)? else {
            return Ok(false);
        };
        store.enqueue_mirror(account_id, crate::mirror::TELEGRAM, &address, &body, &key, false)
    })
    .await?;
    if queued {
        core.wake_mirror();
    }
    Ok(queued)
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p scout-core move_check 2>&1 | grep -E "^error|FAILED|panicked|test result" -A5 | head -20`
Expected: `test result: ok. 6 passed`.

If `Verdict` is reported as not implementing `Debug` in a tuple assert, the derive is already there; if `RunOutcome` is reported as private, it is `pub` in `crate::run` — check the `use`.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/move_check.rs crates/scout-core/src/lib.rs
git commit -m "feat(core): the check that follows a move — its brief, its verdict, what it writes down

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: `trips::move_item` and `trips::dismiss_warning`

**Files:**
- Modify: `crates/scout-core/src/trips.rs`

- [ ] **Step 1: Write the failing test**

In the `mod tests` of `crates/scout-core/src/trips.rs`, which already has `async fn core() -> (Core, tempfile::TempDir, i64)` (the last is an account id), add:

```rust
    #[tokio::test]
    async fn a_move_from_the_page_answers_with_what_the_check_needs() {
        let (core, _dir, account_id) = core().await;
        seed_trip_for_tests(&core, account_id, "October").await.unwrap();
        seed_item_for_tests(&core, account_id, "October", "activity", "Lunch with Stanley", "2026-10-12").await.unwrap();
        let seen = |title: &str, date: &str| ItemExpectation { title: Some(title.into()), date: Some(date.into()), ..Default::default() };

        let out = move_item(&core, account_id, "october", 2, seen("Lunch with Stanley", "2026-10-12"), "2026-10-14", false).await.unwrap();
        let MoveEdit::Moved(m) = out else { panic!("{out:?}") };
        // The trip's stored name, for the brief.
        assert_eq!((m.trip.as_str(), m.title.as_str(), m.from.as_str(), m.to.as_str()), ("October", "Lunch with Stanley", "2026-10-12", "2026-10-14"));

        let lunch = || seen("Lunch with Stanley", "2026-10-14");
        let same = move_item(&core, account_id, "October", 2, lunch(), "2026-10-14", false).await.unwrap();
        assert!(matches!(same, MoveEdit::Same), "{same:?}");
        let bad = move_item(&core, account_id, "October", 2, lunch(), "14 Oct", false).await.unwrap();
        assert!(matches!(bad, MoveEdit::Invalid(_)), "{bad:?}");
        let stale = move_item(&core, account_id, "October", 2, seen("Dinner", "2026-10-14"), "2026-10-15", false).await.unwrap();
        assert!(matches!(stale, MoveEdit::SegmentChanged), "{stale:?}");
        let lost = move_item(&core, account_id, "Nowhere", 2, lunch(), "2026-10-15", false).await.unwrap();
        assert!(matches!(lost, MoveEdit::TripNotFound), "{lost:?}");

        seed_warning_for_tests(&core, account_id, "October", "Lunch with Stanley", "closed that day").await.unwrap();
        let out = dismiss_warning(&core, account_id, "October", 2, seen("Lunch with Stanley", "2026-10-14")).await.unwrap();
        let LegEdit::Done(plan) = out else { panic!("{out:?}") };
        assert_eq!(plan.trip.items[1].warning, None);
    }
```

Run: `cargo test -p scout-core a_move_from_the_page 2>&1 | grep -E "^error" -A3 | head -8`
Expected: `cannot find function move_item`.

- [ ] **Step 2: Implement**

In `trips.rs`, directly above `fn checked_expectation`, add:

```rust
/// What a move asked from the page came to.
#[derive(Debug)]
pub enum MoveEdit {
    /// Moved; what the check that follows needs to know.
    Moved(Box<crate::move_check::Move>),
    Same,
    /// Held, and the request did not say to move it anyway.
    NeedsConfirm,
    TripNotFound,
    SegmentChanged,
    Invalid(String),
}

/// Move one item to another day, if it is still the item the caller drew.
///
/// `to` is checked before the trip is read: a date that is not one is
/// refused whatever it was aimed at.
pub async fn move_item(
    core: &Core,
    account_id: i64,
    trip_name: &str,
    position: i64,
    expected: ItemExpectation,
    to: &str,
    confirm: bool,
) -> anyhow::Result<MoveEdit> {
    if chrono::NaiveDate::parse_from_str(to, "%Y-%m-%d").is_err() {
        return Ok(MoveEdit::Invalid("to is a date, YYYY-MM-DD".to_string()));
    }
    let (origin, destination, date, title) = match checked_expectation(expected) {
        Ok(parts) => parts,
        Err(message) => return Ok(MoveEdit::Invalid(message)),
    };
    let store = core.store();
    let (trip_name, to) = (trip_name.to_string(), to.to_string());
    blocking(move || {
        let Some(trip) = store.find_trip(account_id, &trip_name)? else {
            return Ok(MoveEdit::TripNotFound);
        };
        let expected = ExpectedItem {
            origin: origin.as_deref(),
            destination: destination.as_deref(),
            title: title.as_deref(),
            date: date.as_deref(),
        };
        Ok(match store.move_item_checked(trip.id, position, expected, &to, confirm)? {
            crate::store::Moved::Stale => MoveEdit::SegmentChanged,
            crate::store::Moved::Flight => {
                MoveEdit::Invalid("a flight's date is its ticket's; ask Scout to change the leg".to_string())
            }
            crate::store::Moved::Same => MoveEdit::Same,
            crate::store::Moved::NeedsConfirm => MoveEdit::NeedsConfirm,
            crate::store::Moved::Done { item_id, title, from } => MoveEdit::Moved(Box::new(crate::move_check::Move {
                account_id,
                item_id,
                trip: trip.name,
                title,
                from,
                to,
            })),
        })
    })
    .await
}

/// Clear the warning on one item, if it is still the item the caller drew.
pub async fn dismiss_warning(
    core: &Core,
    account_id: i64,
    trip_name: &str,
    position: i64,
    expected: ItemExpectation,
) -> anyhow::Result<LegEdit> {
    let (origin, destination, date, title) = match checked_expectation(expected) {
        Ok(parts) => parts,
        Err(message) => return Ok(LegEdit::Invalid(message)),
    };
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
        if !store.dismiss_warning_checked(trip.id, position, expected)? {
            return Ok(LegEdit::SegmentChanged);
        }
        let Some(trip) = store.find_trip(account_id, &trip_name)? else {
            return Ok(LegEdit::TripNotFound);
        };
        let chat = store.trip_chat(trip.id)?;
        Ok(LegEdit::Done(Box::new(Plan::from_trip(trip, chat))))
    })
    .await
}
```

Next to `seed_item_for_tests`, add:

```rust
/// A warning on an item, as a check would have left it, without a model.
#[doc(hidden)]
pub async fn seed_warning_for_tests(
    core: &Core,
    account_id: i64,
    trip_name: &str,
    title: &str,
    warning: &str,
) -> anyhow::Result<()> {
    let store = core.store();
    let (trip_name, title, warning) = (trip_name.to_string(), title.to_string(), warning.to_string());
    blocking(move || {
        let trip = store.find_trip(account_id, &trip_name)?.ok_or_else(|| anyhow::anyhow!("no such trip"))?;
        let item = trip.items.iter().find(|i| i.title == title).ok_or_else(|| anyhow::anyhow!("no such item"))?;
        store.finish_item_check(item.id, &item.date, Some(&warning))?;
        Ok(())
    })
    .await
}
```

- [ ] **Step 3: Run the test**

Run: `cargo test -p scout-core a_move_from_the_page 2>&1 | grep -E "^error|test result|panicked" -A5 | head -12`
Expected: `1 passed`.

- [ ] **Step 4: Commit**

```bash
git add crates/scout-core/src/trips.rs
git commit -m "feat(core): move an item and dismiss its warning, for the page

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: The routes, and the task that runs the check

**Files:**
- Modify: `crates/scout-web/src/routes/trips.rs`
- Modify: `crates/scout-web/src/routes/chat.rs` (two visibilities)
- Modify: `docs/superpowers/specs/2026-10-02-in-trip-mode-design.md` (one sentence)

- [ ] **Step 1: Write the failing tests**

In the `mod tests` of `routes/trips.rs`, add:

```rust
    async fn with_lunch() -> (axum::Router, std::sync::Arc<scout_core::core::Core>, tempfile::TempDir, i64, String, String) {
        let (app, core, dir, account_id, cookie, csrf) = setup().await;
        scout_core::trips::seed_item_for_tests(&core, account_id, "October", "activity", "Lunch with Stanley", "2026-10-12")
            .await
            .unwrap();
        (app, core, dir, account_id, cookie, csrf)
    }

    async fn json_of(res: Response) -> serde_json::Value {
        serde_json::from_str(&body_of(res).await).unwrap()
    }

    #[tokio::test]
    async fn a_plan_moves_at_once_and_is_marked_as_being_checked() {
        let (app, _core, _dir, _account, cookie, csrf) = with_lunch().await;
        let body = r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12","to":"2026-10-14"}"#;
        let res = post_json(&app, "/chat/trips/item-move", &cookie, Some(&csrf), body).await;
        assert_eq!(res.status(), StatusCode::OK);
        let answer = json_of(res).await;
        assert_eq!((answer["moved"].as_bool(), answer["checked"].as_bool()), (Some(true), Some(true)));
        let lunch = answer["trip"]["items"].as_array().unwrap().iter().find(|i| i["title"] == "Lunch with Stanley").unwrap();
        assert_eq!(lunch["date"], "2026-10-14");
        assert_eq!(lunch["checking"], true, "the card says so from this response, not five seconds later");

        // The same request again names a day the item is no longer on.
        let res = post_json(&app, "/chat/trips/item-move", &cookie, Some(&csrf), body).await;
        assert_eq!(res.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn a_held_item_is_not_moved_until_the_request_says_so() {
        let (app, _core, _dir, _account, cookie, csrf) = with_lunch().await;
        let held = r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12","held":true}"#;
        assert_eq!(post_json(&app, "/chat/trips/item-held", &cookie, Some(&csrf), held).await.status(), StatusCode::OK);

        let ask = r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12","to":"2026-10-14"}"#;
        let res = post_json(&app, "/chat/trips/item-move", &cookie, Some(&csrf), ask).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(json_of(res).await, serde_json::json!({ "needs_confirm": true }));
        let trips = json_of(get_with_cookie(&app, "/chat/trips", &cookie).await).await;
        assert_eq!(trips[0]["items"][1]["date"], "2026-10-12", "asking is not moving");

        let sure = r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12","to":"2026-10-14","confirm":true}"#;
        let answer = json_of(post_json(&app, "/chat/trips/item-move", &cookie, Some(&csrf), sure).await).await;
        assert_eq!(answer["moved"], true);
    }

    #[tokio::test]
    async fn what_cannot_be_moved_is_refused_and_a_move_to_the_same_day_is_nothing() {
        let (app, _core, _dir, _account, cookie, csrf) = with_lunch().await;
        let to = "/chat/trips/item-move";
        let flight = r#"{"trip":"October","position":1,"title":"AMS → LIS","date":"2026-10-12","to":"2026-10-13"}"#;
        assert_eq!(post_json(&app, to, &cookie, Some(&csrf), flight).await.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bad_date = r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12","to":"tomorrow"}"#;
        assert_eq!(post_json(&app, to, &cookie, Some(&csrf), bad_date).await.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let same = r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12","to":"2026-10-12"}"#;
        let answer = json_of(post_json(&app, to, &cookie, Some(&csrf), same).await).await;
        assert_eq!((answer["moved"].as_bool(), answer["checked"].as_bool()), (Some(false), Some(false)));
        let lost = r#"{"trip":"Nowhere","position":2,"title":"Lunch with Stanley","date":"2026-10-12","to":"2026-10-13"}"#;
        assert_eq!(post_json(&app, to, &cookie, Some(&csrf), lost).await.status(), StatusCode::NOT_FOUND);
        let good = r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12","to":"2026-10-13"}"#;
        assert_eq!(post_json(&app, to, &cookie, None, good).await.status(), StatusCode::BAD_REQUEST, "no form token, no move");
    }

    #[tokio::test]
    async fn over_the_daily_cap_the_item_moves_and_nothing_is_checked() {
        let (app, core, _dir, account_id, cookie, csrf) = with_lunch().await;
        for _ in 0..20 {
            core.log_request(account_id, "text").await.unwrap();
        }
        let body = r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12","to":"2026-10-14"}"#;
        let answer = json_of(post_json(&app, "/chat/trips/item-move", &cookie, Some(&csrf), body).await).await;
        assert_eq!((answer["moved"].as_bool(), answer["checked"].as_bool()), (Some(true), Some(false)));
        let lunch = answer["trip"]["items"].as_array().unwrap().iter().find(|i| i["title"] == "Lunch with Stanley").unwrap();
        assert_eq!(lunch["checking"], false);
    }

    #[tokio::test]
    async fn a_warning_is_dismissed_from_the_page_and_a_stale_card_is_a_conflict() {
        let (app, core, _dir, account_id, cookie, csrf) = with_lunch().await;
        scout_core::trips::seed_warning_for_tests(&core, account_id, "October", "Lunch with Stanley", "closed that day").await.unwrap();
        let trips = json_of(get_with_cookie(&app, "/chat/trips", &cookie).await).await;
        assert_eq!(trips[0]["items"][1]["warning"], "closed that day");

        let stale = r#"{"trip":"October","position":2,"title":"Dinner","date":"2026-10-12"}"#;
        assert_eq!(post_json(&app, "/chat/trips/item-warning", &cookie, Some(&csrf), stale).await.status(), StatusCode::CONFLICT);
        let mine = r#"{"trip":"October","position":2,"title":"Lunch with Stanley","date":"2026-10-12"}"#;
        let plan = json_of(post_json(&app, "/chat/trips/item-warning", &cookie, Some(&csrf), mine).await).await;
        assert_eq!(plan["items"][1]["warning"], serde_json::Value::Null);
    }
```

Run: `cargo test -p scout-web a_plan_moves_at_once 2>&1 | grep -E "test result|panicked" -A4 | head -8`
Expected: FAIL — the route answers 404 or 405.

- [ ] **Step 2: Open the two helpers**

In `crates/scout-web/src/routes/chat.rs`, change `async fn reply_to_for(` to `pub(crate) async fn reply_to_for(` and `async fn queue_conversation(` to `pub(crate) async fn queue_conversation(`.

- [ ] **Step 3: Implement the routes**

In `routes/trips.rs`, in the router, after `.route("/chat/trips/item-held", post(hold_item))` add:

```rust
        .route("/chat/trips/item-move", post(move_item))
        .route("/chat/trips/item-warning", post(dismiss_warning))
```

Directly above `async fn note_item(`, add:

```rust
/// What the page sends to move an item to another day. Named and guarded
/// as `NoteItemIn` is. `confirm` is the answer to "it is held; move it
/// anyway?", and defaults to no.
#[derive(serde::Deserialize)]
struct MoveItemIn {
    trip: String,
    position: i64,
    title: Option<String>,
    date: Option<String>,
    to: String,
    #[serde(default)]
    confirm: bool,
}

/// The trip as it is now, with what happened to the move.
async fn move_answer(auth: &AuthState, account_id: i64, trip: &str, moved: bool, checked: bool) -> Response {
    match scout_core::trips::find(&auth.core, account_id, trip).await {
        Ok(Some(plan)) => axum::Json(serde_json::json!({ "trip": plan, "moved": moved, "checked": checked })).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not read a trip after a move");
            sorry()
        }
    }
}

/// Moves an item, then has Scout check the move.
///
/// The move does not wait for the check and does not depend on it: a plan
/// moves at once, and a held item moves once the request confirms it. The
/// check is a request like any other — logged, counted toward the day —
/// and when the account has none left the item moves unchecked and the
/// answer says so.
async fn move_item(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<MoveItemIn>,
) -> Response {
    use scout_core::trips::MoveEdit;
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let expected = scout_core::trips::ItemExpectation {
        origin: None,
        destination: None,
        title: body.title,
        date: body.date,
    };
    let moved = match scout_core::trips::move_item(&auth.core, account_id, &body.trip, body.position, expected, &body.to, body.confirm).await {
        Ok(MoveEdit::Moved(moved)) => *moved,
        Ok(MoveEdit::Same) => return move_answer(&auth, account_id, &body.trip, false, false).await,
        Ok(MoveEdit::NeedsConfirm) => return axum::Json(serde_json::json!({ "needs_confirm": true })).into_response(),
        Ok(MoveEdit::TripNotFound) => return StatusCode::NOT_FOUND.into_response(),
        Ok(MoveEdit::SegmentChanged) => return StatusCode::CONFLICT.into_response(),
        Ok(MoveEdit::Invalid(message)) => return (StatusCode::UNPROCESSABLE_ENTITY, message).into_response(),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not move a trip item");
            return sorry();
        }
    };
    let checked = scout_core::session::over_daily_cap(&auth.core, account_id).await.is_none()
        && auth.by_account.allow(&format!("check:{account_id}"));
    if checked {
        if let Err(e) = auth.core.log_request(account_id, "move_check").await {
            tracing::warn!(error = %e, account_id, "request logging failed");
        }
        // Before the answer is read, so the trip it carries already says
        // the item is being checked.
        if let Err(e) = scout_core::move_check::begin(&auth.core, moved.item_id).await {
            tracing::warn!(error = %e, account_id, "could not mark an item as being checked");
        }
        tokio::spawn(run_check(auth.clone(), moved.clone()));
    }
    move_answer(&auth, account_id, &moved.trip, true, checked).await
}

/// The thread a check runs in: the trip's own when the web may post into
/// it, else a new one — `composerTarget`'s rule on the page, for the same
/// reason. A Telegram group's thread is other people's room.
async fn check_thread(auth: &AuthState, account_id: i64, trip: &str) -> anyhow::Result<i64> {
    let plan = scout_core::trips::find(&auth.core, account_id, trip)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the trip is gone"))?;
    match plan.chat.as_ref().filter(|chat| chat.scope == "direct") {
        Some(chat) => Ok(chat.id),
        None => scout_core::session::reset(&auth.core, account_id, "direct").await,
    }
}

/// The check, as a background task: a run of the main agent in the trip's
/// thread, mirrored to the phone when the thread is.
async fn run_check(auth: AuthState, moved: scout_core::move_check::Move) {
    let account_id = moved.account_id;
    let thread = match check_thread(&auth, account_id, &moved.trip).await {
        Ok(thread) => thread,
        Err(e) => {
            tracing::warn!(error = %e, account_id, "no thread to check a move in");
            scout_core::move_check::abandon(&auth.core, &moved).await;
            return;
        }
    };
    let reply_to = crate::routes::chat::reply_to_for(&auth, account_id).await;
    let verdict = scout_core::move_check::check(&auth.core, &moved, |prompt: String| {
        let (auth, reply_to) = (auth.clone(), reply_to.clone());
        async move {
            let run = scout_api::RunContext {
                account_id,
                conversation_id: thread,
                reply_to,
                // The line the thread shows, which is also what names a
                // thread started for this.
                title_source: prompt.lines().next().map(str::to_string),
            };
            // Nobody is watching this run draw; the events are read so the
            // channel has a reader and dropped.
            let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();
            let drain = tokio::spawn(async move { while seen.recv().await.is_some() {} });
            let outcome = scout_core::run::run_agent(&auth.core, events, &run, &prompt).await;
            let _ = drain.await;
            if matches!(&outcome, Ok(scout_core::run::RunOutcome::Answered(_))) {
                crate::routes::chat::queue_conversation(&auth, account_id, thread).await;
            }
            outcome
        }
    })
    .await;
    tracing::info!(account_id, item_id = moved.item_id, verdict = ?verdict, "a move was checked");
}

/// What the page sends to dismiss an item's warning.
#[derive(serde::Deserialize)]
struct WarningIn {
    trip: String,
    position: i64,
    title: Option<String>,
    date: Option<String>,
}

async fn dismiss_warning(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<WarningIn>,
) -> Response {
    let account_id = match admitted_account(&auth, &headers).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    if !csrf_header_ok(&auth, &headers, account_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let expected = scout_core::trips::ItemExpectation {
        origin: None,
        destination: None,
        title: body.title,
        date: body.date,
    };
    match scout_core::trips::dismiss_warning(&auth.core, account_id, &body.trip, body.position, expected).await {
        Ok(out) => leg_response(out),
        Err(e) => {
            tracing::error!(error = %e, account_id, "could not dismiss a warning");
            sorry()
        }
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p scout-web routes::trips 2>&1 | grep -E "^error|FAILED|panicked|test result" -A5 | head -20`
Expected: `test result: ok`.

The background check in these tests runs against a model on a closed port, fails at once and ends the check; nothing in the tests waits for it.

- [ ] **Step 5: Update the spec's sentence**

In `docs/superpowers/specs/2026-10-02-in-trip-mode-design.md`, replace

```markdown
Checked in this order: the stale-tab rule, a flight, `to` not a date, `to`
equal to `date`, then held without `confirm`.
```

with

```markdown
Checked in this order: `to` not a date, the stale-tab rule, a flight, `to`
equal to `date`, then held without `confirm`. A malformed date is refused
before anything is looked up, as every other route here treats a bad field.
```

- [ ] **Step 6: Commit**

```bash
git add crates/scout-web/src/routes docs/superpowers/specs/2026-10-02-in-trip-mode-design.md
git commit -m "feat(web): move an item from the page, and have Scout check the move

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 6: The desk's rule and the printed plan

**Files:**
- Modify: `crates/scout-core/src/flights.rs`
- Modify: `crates/scout-web/src/trip_pdf.rs`

- [ ] **Step 1: Write the failing tests**

In `flights.rs`'s `mod tests`, add:

```rust
    #[test]
    fn the_desk_moves_a_held_item_when_asked_and_says_the_booking_has_not_moved() {
        assert!(FLIGHT_PREAMBLE.contains("the booking itself has not moved"), "the desk refuses or stays silent about a held move");
    }
```

In `trip_pdf.rs`'s `mod tests`, directly after `a_note_is_printed_on_the_item_it_belongs_to_as_text_and_only_as_text`, add:

```rust
    #[test]
    fn a_warning_is_printed_under_the_item_it_is_about() {
        // The page says it in yellow under the card's head; a plan printed
        // the morning of the day should not be the one place it is unsaid.
        let mut flagged = plan();
        flagged.trip.items[1].warning = Some("closed on <Mondays>".to_string());
        let page = html(&flagged);
        assert!(page.contains("<div class=\"warn-line\">Check: closed on &lt;Mondays&gt;</div>"), "{page}");
        assert!(!html(&plan()).contains("class=\"warn-line\""));
    }
```

Run: `cargo test -p scout-core the_desk_moves_a_held 2>&1 | grep "test result"; cargo test -p scout-web a_warning_is_printed 2>&1 | grep "test result"`
Expected: both `FAILED`.

- [ ] **Step 2: Implement**

In `FLIGHT_PREAMBLE` in `flights.rs`, replace

```
with it and the confirmation it was read from, and renumbers the trip. A \
leg's date or route is update_trip_segment's.
```

with

```
with it and the confirmation it was read from, and renumbers the trip. A \
leg's date or route is update_trip_segment's. Asked to move something the \
traveller holds to another day, move it and say in your report that the \
booking itself has not moved - changing a card does not rebook anything.
```

In `trip_pdf.rs`, directly above `fn note(item: &TripItem) -> String {`'s doc comment, add:

```rust
/// What Scout said about this item when it was last moved, if it was a
/// warning. Escaped like every other stored string: the reason is the
/// model's sentence, and a model's sentence is not markup.
fn warning(item: &TripItem) -> String {
    match item.warning.as_deref() {
        Some(reason) => format!("<div class=\"warn-line\">Check: {}</div>", escape(reason)),
        None => String::new(),
    }
}
```

In the non-flight `write!` (the one whose format string ends `</time></div>{}</section>"`), change the end of the format string from `</time></div>{}</section>"` to `</time></div>{}{}</section>"` and replace the argument

```rust
                note(segment)
            )
            .unwrap();
            continue;
```

with

```rust
                warning(segment),
                note(segment)
            )
            .unwrap();
            continue;
```

In the stylesheet string, replace `.note{padding:1.3mm 2.4mm;border-top:1px solid #dbe6e4;color:#50666b;font-size:7.4pt;overflow-wrap:anywhere}` with

```
.note{padding:1.3mm 2.4mm;border-top:1px solid #dbe6e4;color:#50666b;font-size:7.4pt;overflow-wrap:anywhere}.warn-line{padding:1.3mm 2.4mm;border-top:1px solid #dbe6e4;background:#fff9e7;color:#6c5817;font-size:7.4pt;overflow-wrap:anywhere}
```

And in the size guard, replace `            || too_long(&item.notes)` with

```rust
            || too_long(&item.notes)
            || too_long(&item.warning)
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p scout-core flights:: 2>&1 | grep "test result"; cargo test -p scout-web trip_pdf 2>&1 | grep "test result"`
Expected: both `ok`.

- [ ] **Step 4: Commit**

```bash
git add crates/scout-core/src/flights.rs crates/scout-web/src/trip_pdf.rs
git commit -m "feat: the desk's rule for a held move, and the warning on the printed plan

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 7: The page's pure helpers

**Files:**
- Modify: `crates/scout-web/src/chat.js`
- Test: `crates/scout-web/src/chat.test.mjs`

- [ ] **Step 1: Write the failing tests**

Add `moveBody, warningBody, tripDays, dayChipDraggable, shouldPollChecks` to the names imported from `./chat.js`. **Replace** the existing test `the item menu offers Mark as held only on something still to book` with:

```js
// The card's ⋯ menu, in order. Move is for anything but a flight, whose
// date is its ticket's. Held is not offered on something already held.
// Dismiss only where there is a warning to dismiss.
test('the item menu offers what can be done to this item and nothing that cannot', () => {
  assert.deepEqual(itemMenuEntries({ kind: 'activity', title: 'Lunch', booked: false }), ['move', 'hold', 'remove'])
  assert.deepEqual(itemMenuEntries({ kind: 'stay', title: 'Hotel', booked: true }), ['move', 'remove'])
  assert.deepEqual(itemMenuEntries({ kind: 'activity', title: 'Lunch', booked: false, warning: 'closed' }), ['move', 'hold', 'dismiss', 'remove'])
  assert.deepEqual(itemMenuEntries({ kind: 'flight', origin: 'AMS', destination: 'HKG', booked: false }), ['hold', 'remove'])
  assert.deepEqual(itemMenuEntries({ kind: 'flight', origin: 'AMS', destination: 'HKG', booked: true }), ['remove'])
})
```

Append:

```js
test('a move names the item as the page drew it, and says where it goes', () => {
  const item = { position: 3, kind: 'activity', title: 'Lunch', date: '2026-09-24' }
  assert.deepEqual(JSON.parse(moveBody('Hong Kong', item, '2026-09-23')), {
    trip: 'Hong Kong', position: 3, title: 'Lunch', date: '2026-09-24', to: '2026-09-23', confirm: false,
  })
  assert.equal(JSON.parse(moveBody('Hong Kong', item, '2026-09-23', true)).confirm, true)
  assert.deepEqual(JSON.parse(warningBody('Hong Kong', item)), { trip: 'Hong Kong', position: 3, title: 'Lunch', date: '2026-09-24' })
})

test('the day picker lists every day of the trip, the free ones too', () => {
  const trip = {
    items: [
      { position: 1, kind: 'activity', title: 'Moomin', date: '2026-09-22', booked: false },
      { position: 2, kind: 'activity', title: 'Lunch', date: '2026-09-25', starts_at: '2026-09-25T14:30:00', booked: true },
    ],
  }
  const days = tripDays(trip, '2026-09-23', 'en-GB')
  assert.deepEqual(days.map((day) => day.date), ['2026-09-22', '2026-09-23', '2026-09-24', '2026-09-25'])
  assert.deepEqual(days.map((day) => day.today), [false, true, false, false])
  assert.deepEqual(days.map((day) => day.entries.map((entry) => entry.text)), [['Moomin'], [], [], ['Lunch']])
  assert.equal(days[0].label, 'Tue 22')
  assert.deepEqual(tripDays({ items: [] }), [])
})

test('only an item\'s own chip can be dragged, and polling stops when nothing is being checked', () => {
  assert.equal(dayChipDraggable({ type: 'item' }), true)
  assert.equal(dayChipDraggable({ type: 'stay' }), true)
  // A leg's date is its ticket's; an arrival and a check-out are where
  // another entry ends, not things of their own.
  for (const type of ['flight', 'arrival', 'stay-end']) assert.equal(dayChipDraggable({ type }), false)

  const checking = { items: [{ checking: false }, { checking: true }] }
  assert.equal(shouldPollChecks(checking, false), true)
  assert.equal(shouldPollChecks(checking, true), false, 'a hidden tab asks nobody')
  assert.equal(shouldPollChecks({ items: [{ checking: false }] }, false), false)
  assert.equal(shouldPollChecks(undefined, false), false)
})

test('a row carries the mark of an item Scout flagged', () => {
  const rows = tripDayRows({ items: [
    { position: 1, kind: 'activity', title: 'Moomin', date: '2026-09-22', booked: false, warning: 'closed on Tuesdays' },
    { position: 2, kind: 'activity', title: 'Lunch', date: '2026-09-22', booked: false },
  ] }, 'en-GB')
  assert.deepEqual(rows[0].entries.map((entry) => Boolean(entry.warn)), [true, false])
})
```

Run: `node --test crates/scout-web/src/chat.test.mjs 2>&1 | grep -E "^# (pass|fail)|SyntaxError"`
Expected: a SyntaxError naming `moveBody`.

- [ ] **Step 2: Implement**

Replace

```js
export function itemMenuEntries(item) {
  return item.booked ? ['remove'] : ['hold', 'remove']
}
```

with

```js
export function itemMenuEntries(item) {
  const entries = []
  // Not a flight: a leg's date is its ticket's, and Scout's to change.
  if (item.kind !== 'flight') entries.push('move')
  if (!item.booked) entries.push('hold')
  if (item.warning) entries.push('dismiss')
  entries.push('remove')
  return entries
}

// What the page sends to move an item to another day. The item is named
// the way `noteBody` names one, for the same reason. `confirm` answers
// "it is held; move it anyway?", which the server asks whatever the page
// thinks the item is.
export function moveBody(tripName, item, to, confirm = false) {
  return JSON.stringify({
    trip: tripName,
    position: item.position,
    title: item.title ?? null,
    date: item.date ?? null,
    to,
    confirm: Boolean(confirm),
  })
}

export function warningBody(tripName, item) {
  return JSON.stringify({
    trip: tripName,
    position: item.position,
    title: item.title ?? null,
    date: item.date ?? null,
  })
}

// Every day of the trip, first to last, for the picker a move chooses
// from — the free ones too, which is what the overview counts rather than
// draws and exactly where a moved item is most likely to go.
export function tripDays(trip, today = null, locale = undefined) {
  const drawn = tripDayRows(trip, locale).filter((row) => row.kind === 'day')
  if (!drawn.length) return []
  const byDate = new Map(drawn.map((row) => [row.date, row]))
  return eachDay(drawn[0].date, drawn[drawn.length - 1].date).map((date) => ({
    date,
    label: dayLabel(date, locale),
    entries: byDate.get(date)?.entries ?? [],
    today: date === today,
  }))
}

// Whether a chip in the day rows is the item itself. An arrival and a
// check-out are where another entry ends, and a flight does not move.
export function dayChipDraggable(entry) {
  return entry.type === 'item' || entry.type === 'stay'
}

// How often the page asks whether a check has finished, and whether it
// should be asking at all.
export const CHECK_POLL_MS = 5000
export function shouldPollChecks(trip, hidden) {
  return !hidden && Boolean(trip?.items?.some((item) => item.checking))
}
```

In `tripDayRows`, give the two entries that are an item of their own the mark. Replace

```js
      put(item.date, { type: 'stay', time: '', text: item.title, nights: nightsBetween(item.date, ends), position: item.position, held })
```

with

```js
      put(item.date, { type: 'stay', time: '', text: item.title, nights: nightsBetween(item.date, ends), position: item.position, held, ...flagged(item) })
```

and replace

```js
    put(item.date, { type: 'item', time: clockLabel(item.starts_at), text: item.title, position: item.position, held })
```

with

```js
    put(item.date, { type: 'item', time: clockLabel(item.starts_at), text: item.title, position: item.position, held, ...flagged(item) })
```

and directly above `function isDay(value) {` add:

```js
// Only where true, so an entry without it is the entry the shared
// `day_rows.json` cases describe.
function flagged(item) {
  return item.warning ? { warn: true } : {}
}
```

- [ ] **Step 3: Run the tests**

Run: `node --test 'crates/scout-web/src/*.test.mjs' 2>&1 | grep -E "^# (pass|fail)"`
Expected: `# fail 0`.

- [ ] **Step 4: Commit**

```bash
git add crates/scout-web/src/chat.js crates/scout-web/src/chat.test.mjs
git commit -m "feat(trips): what the page needs to move an item — the body, the days, the rules

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 8: The menu, the held confirmation, and the card's states

**Files:**
- Modify: `crates/scout-web/src/chat.js` (inside `start()`)
- Modify: `crates/scout-web/src/chat.html`
- Test: `crates/scout-web/src/chat.test.mjs`

- [ ] **Step 1: Write the failing test**

Append to `chat.test.mjs`:

```js
// The script names these and the stylesheet has to have them, or a card
// being checked looks like any other and a warning is grey text.
test('the page has looks for a card being checked, a warning and the day picker', () => {
  const page = readFileSync(new URL('./chat.html', import.meta.url), 'utf8')
  for (const rule of ['.item-checking{', '.item-warning{', '.item-menu-days{', '.day-chip.warn', '.day-row.drop .day-label']) {
    assert.ok(page.includes(rule), `${rule} is missing from the stylesheet`)
  }
  const script = readFileSync(new URL('./chat.js', import.meta.url), 'utf8')
  assert.match(script, /Moving the card won't move the booking\./)
  assert.match(script, /'\/chat\/trips\/item-move'/)
})
```

Run: `node --test crates/scout-web/src/chat.test.mjs 2>&1 | grep -E "^# (pass|fail)"`
Expected: `# fail 1`.

- [ ] **Step 2: The stylesheet**

In `chat.html`, directly after the rule `  .item-menu-confirm button.danger{border-color:var(--red)}`, add:

```css
  /* The day picker a move chooses from: every day of the trip, so it
     scrolls inside the menu rather than making the menu taller than the
     window. The date leads and what is already on the day follows it,
     quieter, because the date is what is being chosen. */
  .item-menu-days{display:flex; flex-direction:column; gap:2px; max-height:min(52vh, 320px); overflow-y:auto}
  .item-menu-days button{display:flex; align-items:baseline; gap:10px}
  .menu-day{flex:none; min-width:4.6em; color:var(--base2)}
  .menu-day-what{min-width:0; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; color:var(--base01);
    font-size:12px; font-weight:400}
  .item-menu-days button[aria-current="date"]{opacity:.55; cursor:default}
  .item-menu-other{display:flex; align-items:center; justify-content:space-between; gap:10px; padding:8px 11px 4px;
    border-top:1px solid var(--line); margin-top:3px; color:var(--base01); font-size:12px}
  .item-menu-date{background:var(--base03); color:var(--base1); border:1px solid var(--line); border-radius:7px;
    font:inherit; font-size:12px; padding:4px 6px}
  /* Being checked: said beside the state, quietly, because nothing is
     wrong yet. */
  .item-checking{color:var(--base01); font-size:12px; font-style:italic; white-space:nowrap}
  /* The verdict when it is a warning: the yellow the readiness banner
     uses for "needs a decision", under the head where the eye already is. */
  .item-warning{margin:0; padding:9px 16px; border-top:1px solid rgba(181,137,0,.35); background:rgba(181,137,0,.09);
    color:#d1c27e; font-size:13px; line-height:1.45; overflow-wrap:anywhere}
  .day-chip.warn{border-color:var(--yellow)}
  .day-chip.warn::after{content:" !"; color:var(--yellow); font-weight:800}
  .day-row.drop .day-label{color:var(--cyan)}
  .day-row.drop .day-entries{outline:1px dashed var(--cyan); outline-offset:3px; border-radius:7px}
```

In the `@media (max-width:720px)` block, after the rule `    .item-menu-list button{min-height:42px; font-size:14px}`, add:

```css
    .item-menu-date{min-height:42px; font-size:16px}
```

- [ ] **Step 3: The menu**

In `chat.js` inside `start()`, next to `let todayCards = new Set()`, add:

```js
  // Each card's menu, by the item's position, for a move that begins
  // somewhere else — a chip dropped on a day — and has to ask a held
  // item's question in that item's own menu. Rebuilt with every paint.
  const itemMenus = new Map()
```

In `renderTripDetail`, change

```js
    tripDetail.replaceChildren()
    todayCards = new Set(todayPositions(tripDayRows(trip, undefined, todayOf(trip))))
    // After this function has drawn the cards, which is the rest of it:
    // a microtask runs when the synchronous render is done.
    queueMicrotask(() => openOnToday(trip))
```

to

```js
    tripDetail.replaceChildren()
    itemMenus.clear()
    todayCards = new Set(todayPositions(tripDayRows(trip, undefined, todayOf(trip))))
    // After this function has drawn the cards, which is the rest of it:
    // a microtask runs when the synchronous render is done.
    queueMicrotask(() => {
      openOnToday(trip)
      watchChecks()
    })
```

In `itemMenu`, directly after the line `    trigger.popoverTargetElement = menu`, add:

```js
    // Where the menu is placed. The ⋯ it belongs to, except when a chip
    // dropped on a day opens it to ask a held item's question there.
    let anchor = trigger
```

Replace the whole of `const drawEntries = () => { … }` with:

```js
    const drawEntries = () => {
      menu.setAttribute('aria-label', itemName(item))
      const make = {
        move: () => entry('Move to…', () => {
          drawDays()
          menu.querySelector('button:not(:disabled)')?.focus()
        }),
        hold: () => entry('Mark as held', () => {
          menu.hidePopover()
          holdItem(trip, item, true)
        }),
        dismiss: () => entry('Dismiss warning', () => {
          menu.hidePopover()
          dismissWarning(trip, item)
        }),
        remove: () => entry('Remove', () => {
          drawConfirm()
          menu.querySelector('button')?.focus()
        }, true),
      }
      menu.replaceChildren(...itemMenuEntries(item).map((name) => make[name]()))
    }
    // The trip's days, in place of the list the entry was chosen from, as
    // Remove's question is. Every day, the free ones too. The item's own
    // day is there and dead: a list with a hole where today's row should
    // be is harder to read than one that says "you are here".
    const drawDays = () => {
      const heading = item.kind === 'stay' ? 'Move check-in to…' : 'Move to…'
      menu.setAttribute('aria-label', heading)
      const title = node('p', 'item-menu-ask', heading)
      title.setAttribute('role', 'none')
      const list = node('div', 'item-menu-days')
      for (const day of tripDays(trip, localToday())) {
        const what = day.entries.length ? day.entries.map((dayEntry) => dayEntry.text).join(', ') : 'Nothing planned'
        const button = entry('', () => pickDay(day.date))
        button.append(
          node('span', 'menu-day', day.today ? `${day.label} · today` : day.label),
          node('span', 'menu-day-what', what),
        )
        if (day.date === item.date) {
          button.disabled = true
          button.setAttribute('aria-current', 'date')
        }
        list.append(button)
      }
      // A day outside the trip: the native field, which every phone has
      // a good picker for.
      const other = document.createElement('input')
      other.type = 'date'
      other.className = 'item-menu-date'
      other.setAttribute('aria-label', 'Another date')
      other.addEventListener('change', () => {
        if (other.value) pickDay(other.value)
      })
      const otherRow = node('label', 'item-menu-other', 'Another date…')
      otherRow.append(other)
      menu.replaceChildren(title, list, otherRow)
      // Taller than the list it replaced, so it may no longer fit below.
      flipMenu(menu, anchor)
    }
    const pickDay = (to) => {
      if (to === item.date) return
      if (item.booked) {
        drawHeld(to)
        menu.querySelector('button')?.focus()
        return
      }
      menu.hidePopover()
      moveItem(trip, item, to, false)
    }
    // A held item's question. The card moving does not move the booking,
    // and that is the whole of what there is to say — no model is asked
    // before the move, so nothing is waited for.
    const drawHeld = (to) => {
      const ask = `Held for ${dateLabel(item.date, true)}. Moving the card won't move the booking.`
      menu.setAttribute('aria-label', ask)
      const text = node('p', 'item-menu-ask', ask)
      text.setAttribute('role', 'none')
      const cancel = entry('Cancel', () => {
        drawEntries()
        menu.querySelector('button')?.focus()
      })
      const go = entry('Move anyway', () => {
        menu.hidePopover()
        moveItem(trip, item, to, true)
      }, true)
      // A double-click on a day would otherwise land its second half here.
      armConfirm(go)
      const row = node('div', 'item-menu-confirm')
      row.append(cancel, go)
      menu.replaceChildren(text, row)
    }
```

In the same function, change `      if (event.newState === 'open') placeMenu(menu, trigger)` to `      if (event.newState === 'open') placeMenu(menu, anchor)`, change `      flipMenu(menu, trigger)` to `      flipMenu(menu, anchor)`, and replace

```js
      if (!open) {
        drawEntries()
        return
      }
```

with

```js
      if (!open) {
        anchor = trigger
        drawEntries()
        return
      }
```

Directly before the line `    drawEntries()` that precedes `wrap.append(trigger, menu)`, add:

```js
    itemMenus.set(item.position, {
      // Opens this menu on the held question, placed at `at` — the day
      // row a chip was dropped on — or at the ⋯ when there is no `at`.
      askHeld(to, at) {
        anchor = at ?? trigger
        menu.showPopover()
        drawHeld(to)
      },
    })
```

- [ ] **Step 4: The move, the dismissal, and the watch**

Directly above `async function holdItem`, add:

```js
  // Moves an item to another day. The server moves a plan at once and
  // asks about a held one; either way Scout's check follows the move, and
  // this does not wait for it.
  async function moveItem(trip, item, to, confirm) {
    if (tripChoicePending) return
    tripChoicePending = true
    tripLoadSeq++
    tripDetail.setAttribute('aria-busy', 'true')
    try {
      const res = await fetch('/chat/trips/item-move', {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'x-scout-csrf': csrfToken },
        body: moveBody(trip.name, item, to, confirm),
      })
      if (res.status === 409) {
        tripChoicePending = false
        tripsLoaded = false
        await loadTrips()
        showTripToast('This trip changed elsewhere. Showing the current itinerary.')
        return
      }
      if (res.status === 422) {
        showTripToast(await res.text())
        return
      }
      if (!res.ok) throw new Error('refused')
      const answer = await res.json()
      if (answer.needs_confirm) {
        // The page drew it as a plan and the server knows it is held:
        // somebody marked it elsewhere. Ask, in its own menu.
        itemMenus.get(item.position)?.askHeld(to, null)
        return
      }
      const updated = answer.trip
      trips = [updated, ...trips.filter((other) => other.name !== updated.name)]
      currentTrip = updated.name
      renderTripList()
      renderTripDetail()
      if (!answer.moved) return
      showTripToast(answer.checked
        ? `Moved to ${dateLabel(to, true)}.`
        : `Moved to ${dateLabel(to, true)}. Not checked: daily limit reached.`)
      // To where it went: its position changed with its day, and a reader
      // left looking at the gap has to go and find it.
      const now = updated.items.find((other) => other.kind === item.kind && other.title === item.title && other.date === to)
      const card = now ? document.getElementById(`trip-item-${now.position}`) : null
      card?.scrollIntoView({ block: 'start' })
      card?.focus({ preventScroll: true })
    } catch {
      showTripToast('Could not move that. Try again.')
    } finally {
      tripChoicePending = false
      tripDetail.removeAttribute('aria-busy')
    }
  }

  async function dismissWarning(trip, item) {
    if (tripChoicePending) return
    tripChoicePending = true
    tripLoadSeq++
    tripDetail.setAttribute('aria-busy', 'true')
    try {
      const res = await fetch('/chat/trips/item-warning', {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'x-scout-csrf': csrfToken },
        body: warningBody(trip.name, item),
      })
      if (res.status === 409) {
        tripChoicePending = false
        tripsLoaded = false
        await loadTrips()
        showTripToast('This trip changed elsewhere. Showing the current itinerary.')
        return
      }
      if (!res.ok) throw new Error('refused')
      const updated = await res.json()
      trips = [updated, ...trips.filter((other) => other.name !== updated.name)]
      currentTrip = updated.name
      renderTripList()
      renderTripDetail()
    } catch {
      showTripToast('Could not change that. Try again.')
    } finally {
      tripChoicePending = false
      tripDetail.removeAttribute('aria-busy')
    }
  }

  // Asks again while a check is running, so its verdict reaches the card
  // without a reload. One timer, restarted by every paint; it stops
  // itself when nothing on the trip is being checked or the tab is
  // hidden. A tick is skipped while a menu is open or a note is being
  // typed: a repaint would close the one and throw away the other.
  let checkTimer = null
  function watchChecks() {
    clearTimeout(checkTimer)
    checkTimer = null
    const trip = trips.find((item) => item.name === currentTrip)
    if (!shouldPollChecks(trip, document.hidden)) return
    checkTimer = setTimeout(async () => {
      checkTimer = null
      const busy = tripChoicePending || tripDetail.querySelector(':popover-open, .item-note-editor')
      if (!busy) await loadTrips().catch(() => {})
      watchChecks()
    }, CHECK_POLL_MS)
  }
  document.addEventListener('visibilitychange', watchChecks)
```

- [ ] **Step 5: The card's states**

In `renderOtherItem`, replace

```js
    actions.append(statePill(item), node('time', 'segment-date', itemDateLabel(item)))
    head.append(about, actions, itemMenu(trip, item))
    card.append(head)
    card.append(noteSlot(trip, item))
```

with

```js
    // While Scout is looking at a move: beside the state, since it is
    // about the item and not yet about anything being wrong.
    if (item.checking) actions.append(node('span', 'item-checking', 'Checking…'))
    actions.append(statePill(item), node('time', 'segment-date', itemDateLabel(item)))
    head.append(about, actions, itemMenu(trip, item))
    card.append(head)
    if (item.warning) {
      const warning = node('p', 'item-warning', item.warning)
      warning.setAttribute('role', 'note')
      card.append(warning)
    }
    card.append(noteSlot(trip, item))
```

In `dayChip`, directly after the line `    chip.textContent = \`${time}${entry.text}${tail}\``, add:

```js
    if (entry.warn) {
      chip.classList.add('warn')
      // The "!" is drawn by the stylesheet; this is the same thing said
      // to somebody who is not looking at it.
      chip.setAttribute('aria-label', `${chip.textContent}, flagged by Scout`)
    }
```

- [ ] **Step 6: Run the tests**

Run: `node --test 'crates/scout-web/src/*.test.mjs' 2>&1 | grep -E "^# (pass|fail)"`
Expected: `# fail 0`.

- [ ] **Step 7: Commit**

```bash
git add crates/scout-web/src/chat.js crates/scout-web/src/chat.html crates/scout-web/src/chat.test.mjs
git commit -m "feat(trips): Move to… in the item menu, the held question, and what a check leaves on the card

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 9: Dragging a chip to another day

**Files:**
- Modify: `crates/scout-web/src/chat.js` (inside `start()`: `renderOverview`, `dayChip`)

- [ ] **Step 1: Implement**

Dragging has no pure part left to test that Task 7 did not cover (`dayChipDraggable`); it is verified in a browser in Task 10.

Inside `start()`, next to `const itemMenus = new Map()`, add:

```js
  // A mouse, not a finger: on touch a chip is a link to its card, and a
  // long press that starts a drag would fight the row's own scrolling.
  // The menu is how a phone and a keyboard move things.
  const canDrag = Boolean(window.matchMedia?.('(hover: hover) and (pointer: fine)')?.matches)
  // The position of the chip being dragged, or `null`. A flight's chip is
  // an anchor and so can be dragged as a link whatever this page says;
  // that drag leaves this `null`, and no row accepts it.
  let dragged = null
```

`dayChip` needs the row's date only to refuse a drop on its own day, which the row does; it needs nothing new passed in. In `dayChip`, directly before `    return chip`, add:

```js
    if (canDrag && dayChipDraggable(entry)) {
      chip.draggable = true
      chip.addEventListener('dragstart', (event) => {
        dragged = entry.position
        event.dataTransfer.effectAllowed = 'move'
        event.dataTransfer.setData('text/plain', String(entry.position))
      })
      chip.addEventListener('dragend', () => {
        dragged = null
        for (const row of tripDetail.querySelectorAll('.day-row.drop')) row.classList.remove('drop')
      })
    }
```

In `renderOverview`, directly after the line `      line.append(label)` (added by slice 1), add:

```js
      // A day is somewhere an item can be dropped. The row is
      // `display:contents`, so it has no box of its own, but its children
      // do and their events bubble to it.
      if (canDrag) {
        line.addEventListener('dragover', (event) => {
          if (dragged === null) return
          event.preventDefault()
          event.dataTransfer.dropEffect = 'move'
          line.classList.add('drop')
        })
        line.addEventListener('dragleave', (event) => {
          if (!line.contains(event.relatedTarget)) line.classList.remove('drop')
        })
        line.addEventListener('drop', (event) => {
          event.preventDefault()
          line.classList.remove('drop')
          const item = trip.items.find((candidate) => candidate.position === dragged)
          dragged = null
          if (!item || item.date === row.date) return
          if (item.booked) itemMenus.get(item.position)?.askHeld(row.date, label)
          else moveItem(trip, item, row.date, false)
        })
      }
```

A counted run of free days ("8 free days") is one line and not a day, so nothing can be dropped on it; the menu's picker lists those days.

- [ ] **Step 2: Run the tests**

Run: `node --test 'crates/scout-web/src/*.test.mjs' 2>&1 | grep -E "^# (pass|fail)"`
Expected: `# fail 0`.

- [ ] **Step 3: Commit**

```bash
git add crates/scout-web/src/chat.js
git commit -m "feat(trips): drag an item's chip to another day, on a desktop

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 10: See it in a browser, then ship

**Files:**
- Modify: `README.md`, `docs/BOARD.md`

- [ ] **Step 1: Extend the stand-in from slice 1**

In the scratch directory slice 1 used (`$SCRATCH/today`), in `server.mjs`, directly above the line `    send(404, 'text/plain', 'no')`, add:

```js
    if (req.url === '/chat/trips/item-move') {
      const b = JSON.parse(body)
      const it = trip.items.find((i) => i.position === b.position && i.title === b.title)
      if (!it) return send(409, 'text/plain', '')
      if (it.booked && !b.confirm) return send(200, 'application/json', JSON.stringify({ needs_confirm: true }))
      it.date = b.to
      if (it.starts_at) it.starts_at = `${b.to}${it.starts_at.slice(10)}`
      it.warning = null
      it.checking = true
      trip.items.sort((x, y) => x.date.localeCompare(y.date)).forEach((x, n) => { x.position = n + 1 })
      // The verdict lands two polls later.
      setTimeout(() => { it.checking = false; it.warning = 'APM is closed that day' }, 7000)
      return send(200, 'application/json', JSON.stringify({ trip, moved: true, checked: true }))
    }
    if (req.url === '/chat/trips/item-warning') {
      const b = JSON.parse(body)
      trip.items.find((i) => i.position === b.position).warning = null
      return send(200, 'application/json', JSON.stringify(trip))
    }
```

Create `move.mjs`:

```js
import { chromium, devices } from 'playwright'
const browser = await chromium.launch()

// The menu path, on a phone, in the Mini App's shape.
{
  const page = await browser.newPage({ ...devices['iPhone 13'], viewport: { width: 390, height: 844 } })
  page.on('pageerror', (e) => console.log('phone pageerror', e.message))
  await page.goto('http://127.0.0.1:8736/chat?in=telegram', { waitUntil: 'networkidle' })
  await page.waitForSelector('.item-card')
  await page.getByRole('button', { name: 'More for Moomin shop' }).click()
  await page.getByRole('menuitem', { name: 'Move to…' }).click()
  console.log('phone days offered:', await page.locator('.item-menu-days:visible button').count())
  await page.screenshot({ path: 'move-picker.png' })
  await page.locator('.item-menu-days:visible button:not(:disabled)').nth(1).click()
  await page.waitForTimeout(300)
  console.log('phone toast:', await page.locator('.trip-toast').innerText())
  console.log('phone checking shown:', await page.locator('.item-checking').count())
  await page.waitForTimeout(12000)
  console.log('phone warning:', await page.locator('.item-warning').innerText())
  await page.screenshot({ path: 'move-warning.png' })
  await page.getByRole('button', { name: 'More for Moomin shop' }).click()
  await page.getByRole('menuitem', { name: 'Dismiss warning' }).click()
  await page.waitForTimeout(300)
  console.log('phone warning gone:', await page.locator('.item-warning').count() === 0)
  // A held item asks first.
  await page.getByRole('button', { name: 'More for Lunch with Stanley' }).click()
  await page.getByRole('menuitem', { name: 'Move to…' }).click()
  await page.locator('.item-menu-days:visible button:not(:disabled)').first().click()
  console.log('phone held question:', await page.locator('.item-menu-ask:visible').innerText())
  await page.screenshot({ path: 'move-held.png' })
  await page.waitForTimeout(600)
  await page.getByRole('menuitem', { name: 'Move anyway' }).click()
  await page.waitForTimeout(300)
  console.log('phone held moved, toast:', await page.locator('.trip-toast').innerText())
  await page.close()
}

// Drag and drop, on a desktop.
{
  const page = await browser.newPage({ viewport: { width: 1360, height: 900 } })
  page.on('pageerror', (e) => console.log('desktop pageerror', e.message))
  await page.goto('http://127.0.0.1:8736/', { waitUntil: 'networkidle' })
  await page.getByRole('button', { name: /^Trips/ }).click()
  await page.waitForSelector('.item-card')
  const chip = page.locator('.day-chip', { hasText: 'Star Ferry' })
  console.log('desktop chip draggable:', await chip.getAttribute('draggable'))
  const target = page.locator('.day-row', { hasText: 'Peak tram' }).locator('.day-entries')
  await chip.dragTo(target)
  await page.waitForTimeout(400)
  console.log('desktop toast after drop:', await page.locator('.trip-toast').innerText())
  await page.screenshot({ path: 'move-drag.png' })
  await page.close()
}
await browser.close()
```

- [ ] **Step 2: Run it**

```bash
(node server.mjs > server.log 2>&1 &) ; sleep 0.5 ; node move.mjs 2>&1 | grep -v 404 ; pkill -f "node server.mjs"
```

Expected:
- `phone days offered:` 16 (the trip's first day to its last)
- `phone toast: Moved to …`
- `phone checking shown: 1`
- `phone warning: APM is closed that day`
- `phone warning gone: true`
- `phone held question: Held for … Moving the card won't move the booking.`
- `phone held moved, toast: Moved to …`
- `desktop chip draggable: true`
- `desktop toast after drop: Moved to …`
- no `pageerror` lines

Open the four screenshots and look: the picker scrolls inside the menu and does not run off the screen; the warning is a yellow line under the card's head; the held question has Cancel and Move anyway side by side.

- [ ] **Step 3: If anything is off**

Fix it in `chat.js` or `chat.html`, re-run Step 2, and commit the fix with a message that says what was wrong. Two likely ones: the picker opening off the bottom of the window (the `flipMenu(menu, anchor)` at the end of `drawDays`), and a drop doing nothing because `dragover` did not call `preventDefault` (the `dragged === null` guard firing — check `dragstart` ran).

- [ ] **Step 4: README**

In `README.md`, directly after the paragraph slice 1 added (it ends `so "move the ferry to tomorrow" has a tomorrow.`), add:

```markdown
Anything but a flight can be moved to another day, from **Move to…** in its
⋯ menu or, on a desktop, by dragging its chip onto another day's row. A stay
moves as a block and keeps its nights. A plan moves at once; a held item asks
first, because moving the card does not move the booking. Either way Scout
then checks the move — are you in that city that day, does it clash with
something timed, is the place open — and a problem appears as a line on the
card and a message on your phone. The check counts as one request.
```

- [ ] **Step 5: The gate**

```bash
cargo test --workspace 2>&1 | grep -E "FAILED|panicked|test result" | sort | uniq -c
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -E "^error" -A6 | head
node --test 'crates/scout-web/src/*.test.mjs' 2>&1 | grep -E "^# (pass|fail)"
```

Expected: every `test result` line says `ok`, no clippy output, `# fail 0`.

- [ ] **Step 6: Commit, merge, push**

```bash
git add README.md
git commit -m "docs: moving an item between days

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
git checkout main
git merge --no-ff feat/in-trip-move -m "Merge: move an item between days, and have Scout check the move

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
git push origin main
git branch -d feat/in-trip-move
```

- [ ] **Step 7: Deploy and verify the migration**

This deploy runs schema step 20 on production's `trip_items`, which has rows. Step 19's first attempt crash-looped the pod; this is the step to watch.

```bash
scripts/deploy-k3s.sh
ssh $(sed -n 's/^SCOUT_SSH=//p' .env) 'kubectl -n scout get pods -l app=scout; kubectl -n scout logs -l app=scout --tail=60' | grep -E "Running|Crash|migration|schema=|scout is up|Telegram delivers|Error"
```

Expected: `applied migration step step=20`, `schema=20`, the pod `Running`, `scout is up`, `Telegram delivers here`. If the pod is in `CrashLoopBackOff`, read `kubectl -n scout logs -l app=scout --tail=30 --previous` for the migration error and fix forward on a branch; the store backed the database up before the attempt.

- [ ] **Step 8: Try it for real**

On the live site, on a trip with an activity that is not held: open its ⋯ menu, **Move to…**, pick a day. Expected: the card moves, the toast says "Moved to …", the card says "Checking…", and within about a minute either the "Checking…" goes with nothing else (fine) or a yellow line appears. Open the trip's chat thread: the check is there as "Moved … Check it." and Scout's reply ending in a `verdict:` line.

Then read the pod's log for the check:

```bash
ssh $(sed -n 's/^SCOUT_SSH=//p' .env) 'kubectl -n scout logs -l app=scout --since=10m' | grep "a move was checked"
```

Expected: one line with `verdict=Fine` or `verdict=Warn(…)`. `verdict=Unknown` on a run that answered means the model did not end with the verdict line; note it, and if it repeats, the brief's last sentence is where to look.

- [ ] **Step 9: The board**

In `docs/BOARD.md`, directly under `## Done`, add one line, with `<hash>` replaced by the merge commit's short hash:

```markdown
- [x] 2026-10-02 — **Move an item between days** (`<hash>`): anything but a flight moves to another day from Move to… in its ⋯ menu, or by dragging its chip on a desktop; a stay moves as a block and keeps its nights. A plan moves at once; a held item asks first, a rule the server keeps as well as the page. Scout then checks the move in the trip's own thread — an ordinary run of the main agent, since the desk cannot search the web — and its reply ends with a verdict line; a warning is written on the item (schema 20), shown on the card and the printed plan, and sent to the phone once per item and day. A verdict about a day the item has since left is not written. The check counts toward the day, and so now does a PDF sent to the bot, which had been logged under a kind the cap did not count.
```

Commit and push:

```bash
git add docs/BOARD.md
git commit -m "docs(board): move an item between days

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
git push origin main
```

Then add the card to the board artifact (`https://claude.ai/code/artifact/9be3e94c-0416-4f18-86a6-528a92847c64`, collection `cards`): a document `in-trip-move` with `area: "trips"`, `column: "done"`, the title "Move an item between days", the commit hash, and the BOARD.md sentence as `detail`.
