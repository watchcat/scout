# Trips: Storage and Building — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A traveller can build, inspect and amend a named multi-city trip from chat, parking several candidate flights on a segment and deciding later.

**Architecture:** Two DuckDB tables behind the existing `Store` (one `Arc<Mutex<Connection>>`, so check-and-write in a single `Store` method is atomic by construction). Six rig tools in a new `src/tools/trips.rs`. Every mutating tool returns the whole trip, so a model addressing the wrong trip is visible in the reply rather than silent.

**Tech Stack:** Rust 2021, duckdb-rs 1.x, rig 0.40 tools, tokio, anyhow, serde.

**Spec:** `docs/superpowers/specs/2026-08-09-trips-design.md`

**Not in this plan:** `finalise_trip` and everything it needs — see `2026-08-09-trips-finalisation.md`. This plan ends with a trip you can build and read back but not price.

---

## File Structure

| File | Responsibility |
|---|---|
| `src/store.rs` (modify) | Schema, `Trip`/`TripSegment`/`TripCandidate`/`NewCandidate` types, all SQL. Must live here: `conn` is private to this module, and that privacy is what makes the atomicity guarantees hold. |
| `src/tools/trips.rs` (create) | The six building tools plus the pure itinerary checks (gaps, short turnarounds, date order). |
| `src/tools/mod.rs` (modify) | Declare the module. |
| `src/agent.rs` (modify) | Register the tools; preamble guidance. |

**Verified before writing this plan** (against DuckDB, so do not re-litigate):
- `UNIQUE (user_id, name_key)` is enforced, and `ON CONFLICT (user_id, name_key) DO NOTHING` works against it.
- `UPDATE ... SET position = position + 1 WHERE position >= ?` then inserting into the hole keeps positions contiguous; `DELETE ... WHERE position = ?` then `position - 1 WHERE position > ?` closes the gap. These are only correct **because** positions are always contiguous — never leave a hole unfilled.

---

### Task 1: Schema and trip records

**Files:**
- Modify: `src/store.rs` (the `MIGRATIONS` const, then new types and methods)

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block at the bottom of `src/store.rs`:

```rust
#[test]
fn a_trip_is_found_by_name_case_insensitively_and_scoped_to_its_owner() {
    let (store, _d) = test_store();
    let trip = store.upsert_trip(7, "September", None, None).unwrap();
    assert_eq!(trip.name, "September");
    assert_eq!(trip.adults, 1, "one adult unless said otherwise");
    assert_eq!(trip.status, "planning");
    assert!(trip.segments.is_empty());

    assert!(store.find_trip(7, "september").unwrap().is_some(), "names are not case-sensitive");
    assert!(store.find_trip(8, "September").unwrap().is_none(), "another user has no such trip");

    // Same name twice is the same trip, not a second one.
    store.upsert_trip(7, "SEPTEMBER", Some(2), Some("business")).unwrap();
    assert_eq!(store.list_trips(7).unwrap().len(), 1);
    let trip = store.find_trip(7, "September").unwrap().unwrap();
    assert_eq!(trip.adults, 2);
    assert_eq!(trip.cabin_class.as_deref(), Some("business"));
    assert_eq!(trip.name, "September", "the original spelling is kept");

    // Two users may each have a "September".
    store.upsert_trip(8, "September", None, None).unwrap();
    assert_eq!(store.list_trips(8).unwrap().len(), 1);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet a_trip_is_found_by_name`
Expected: FAIL to compile — `no method named upsert_trip found for struct Store`.

- [ ] **Step 3: Add the tables**

Append to the `MIGRATIONS` string constant in `src/store.rs`, before the closing `"#;`:

```sql
-- A named plan. The itinerary is durable; prices are not, so nothing here
-- holds an offer id — see the trips design doc.
CREATE SEQUENCE IF NOT EXISTS trips_id_seq;
CREATE TABLE IF NOT EXISTS trips (
    id          BIGINT PRIMARY KEY DEFAULT nextval('trips_id_seq'),
    user_id     BIGINT NOT NULL,
    name        TEXT NOT NULL,
    -- lowercased `name`: the trip is addressed by what the traveller calls
    -- it, and "September" and "september" are the same trip.
    name_key    TEXT NOT NULL,
    adults      BIGINT NOT NULL DEFAULT 1,
    cabin_class TEXT,
    status      TEXT NOT NULL DEFAULT 'planning',
    created_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    UNIQUE (user_id, name_key)
);
-- Where and when. This is all that gets re-searched.
CREATE TABLE IF NOT EXISTS trip_segments (
    trip_id        BIGINT NOT NULL,
    position       BIGINT NOT NULL,
    origin         TEXT NOT NULL,
    destination    TEXT NOT NULL,
    departure_date TEXT NOT NULL,
    PRIMARY KEY (trip_id, position)
);
-- The options on a segment. Several may sit here undecided; at most one
-- carries `chosen`, which is enforced in Rust because `false` repeats.
CREATE TABLE IF NOT EXISTS segment_candidates (
    trip_id            BIGINT NOT NULL,
    position           BIGINT NOT NULL,
    candidate          BIGINT NOT NULL,
    chosen             BOOLEAN NOT NULL DEFAULT false,
    airline            TEXT NOT NULL,
    flight_numbers     TEXT NOT NULL,
    itinerary          TEXT NOT NULL,
    departing_at_local TEXT,
    arriving_at_local  TEXT,
    duration_minutes   BIGINT,
    quoted_price       DOUBLE,
    quoted_currency    TEXT,
    quoted_at          TIMESTAMP,
    source             TEXT,
    PRIMARY KEY (trip_id, position, candidate)
);
```

- [ ] **Step 4: Add the types**

Add near the other record types in `src/store.rs` (after `Reminder`):

```rust
/// A named plan. `id` is not serialised: it is noise to the model, and
/// exposing it invites addressing a trip by something the traveller never
/// said.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Trip {
    #[serde(skip)]
    pub id: i64,
    pub name: String,
    pub adults: i64,
    pub cabin_class: Option<String>,
    /// `planning` or `finalised`. Any edit returns it to `planning`: the
    /// prices it was finalised at stopped describing the trip when the trip
    /// stopped being that trip.
    pub status: String,
    pub segments: Vec<TripSegment>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TripSegment {
    pub position: i64,
    pub origin: String,
    pub destination: String,
    pub departure_date: String,
    pub candidates: Vec<TripCandidate>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TripCandidate {
    pub candidate: i64,
    pub chosen: bool,
    pub airline: String,
    /// Comma-separated and in order (`KL1007,KL0805`), because finalisation
    /// matches this against fresh search results.
    pub flight_numbers: String,
    /// The rendered line `Leg::itinerary` produces, for showing.
    pub itinerary: String,
    pub departing_at_local: Option<String>,
    pub arriving_at_local: Option<String>,
    pub duration_minutes: Option<i64>,
    /// What it cost when parked. Never refreshed — see the design doc.
    pub quoted_price: Option<f64>,
    pub quoted_currency: Option<String>,
    pub source: Option<String>,
}

/// A candidate on its way into the database.
#[derive(Debug, Clone, PartialEq)]
pub struct NewCandidate {
    pub airline: String,
    pub flight_numbers: String,
    pub itinerary: String,
    pub departing_at_local: Option<String>,
    pub arriving_at_local: Option<String>,
    pub duration_minutes: Option<i64>,
    pub quoted_price: Option<f64>,
    pub quoted_currency: Option<String>,
    pub source: Option<String>,
}
```

- [ ] **Step 5: Implement `upsert_trip`, `find_trip`, `list_trips` and the loader**

Add to `impl Store` in `src/store.rs`:

```rust
/// Creates the trip if the name is new, otherwise updates only what was
/// supplied. Two statements rather than one `ON CONFLICT DO UPDATE`: with
/// upsert, an unsupplied `adults` would arrive as the insert's default and
/// overwrite a value already set.
pub fn upsert_trip(
    &self,
    user_id: i64,
    name: &str,
    adults: Option<i64>,
    cabin_class: Option<&str>,
) -> Result<Trip> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("a trip needs a name — ask the traveller what to call it");
    }
    let key = name.to_lowercase();
    let conn = self.conn.lock().unwrap();
    conn.execute(
        "INSERT INTO trips (user_id, name, name_key) VALUES (?, ?, ?)
         ON CONFLICT (user_id, name_key) DO NOTHING",
        params![user_id, name, key],
    )?;
    if let Some(adults) = adults {
        conn.execute(
            "UPDATE trips SET adults = ?, updated_at = current_timestamp
             WHERE user_id = ? AND name_key = ?",
            params![adults, user_id, key],
        )?;
    }
    if let Some(cabin) = cabin_class {
        conn.execute(
            "UPDATE trips SET cabin_class = ?, updated_at = current_timestamp
             WHERE user_id = ? AND name_key = ?",
            params![cabin, user_id, key],
        )?;
    }
    let id: i64 = conn.query_row(
        "SELECT id FROM trips WHERE user_id = ? AND name_key = ?",
        params![user_id, key],
        |row| row.get(0),
    )?;
    load_trip(&conn, id)
}

pub fn find_trip(&self, user_id: i64, name: &str) -> Result<Option<Trip>> {
    let key = name.trim().to_lowercase();
    let conn = self.conn.lock().unwrap();
    let mut stmt =
        conn.prepare("SELECT id FROM trips WHERE user_id = ? AND name_key = ?")?;
    let mut ids = stmt.query_map(params![user_id, key], |row| row.get::<_, i64>(0))?;
    match ids.next() {
        Some(id) => Ok(Some(load_trip(&conn, id?)?)),
        None => Ok(None),
    }
}

/// Every trip this user has, newest activity first.
pub fn list_trips(&self, user_id: i64) -> Result<Vec<Trip>> {
    let conn = self.conn.lock().unwrap();
    let mut stmt = conn
        .prepare("SELECT id FROM trips WHERE user_id = ? ORDER BY updated_at DESC, id DESC")?;
    let ids: Vec<i64> = stmt
        .query_map(params![user_id], |row| row.get(0))?
        .collect::<duckdb::Result<_>>()?;
    ids.into_iter().map(|id| load_trip(&conn, id)).collect()
}
```

And this free function, next to `row_to_purchase` at the bottom of the `impl` block's module:

```rust
/// Reads one whole trip. Takes `&Connection` rather than `&Store` so it can
/// be called by a method that already holds the lock — every trip-mutating
/// method returns the trip it just changed, and re-locking would deadlock.
fn load_trip(conn: &Connection, id: i64) -> Result<Trip> {
    let (name, adults, cabin_class, status) = conn.query_row(
        "SELECT name, adults, cabin_class, status FROM trips WHERE id = ?",
        params![id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        },
    )?;

    let mut stmt = conn.prepare(
        "SELECT position, origin, destination, departure_date FROM trip_segments
         WHERE trip_id = ? ORDER BY position",
    )?;
    let rows: Vec<(i64, String, String, String)> = stmt
        .query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<duckdb::Result<_>>()?;

    let mut stmt = conn.prepare(
        "SELECT position, candidate, chosen, airline, flight_numbers, itinerary,
                departing_at_local, arriving_at_local, duration_minutes,
                quoted_price, quoted_currency, source
         FROM segment_candidates WHERE trip_id = ? ORDER BY position, candidate",
    )?;
    let candidates: Vec<(i64, TripCandidate)> = stmt
        .query_map(params![id], |r| {
            Ok((
                r.get(0)?,
                TripCandidate {
                    candidate: r.get(1)?,
                    chosen: r.get(2)?,
                    airline: r.get(3)?,
                    flight_numbers: r.get(4)?,
                    itinerary: r.get(5)?,
                    departing_at_local: r.get(6)?,
                    arriving_at_local: r.get(7)?,
                    duration_minutes: r.get(8)?,
                    quoted_price: r.get(9)?,
                    quoted_currency: r.get(10)?,
                    source: r.get(11)?,
                },
            ))
        })?
        .collect::<duckdb::Result<_>>()?;

    let segments = rows
        .into_iter()
        .map(|(position, origin, destination, departure_date)| TripSegment {
            position,
            origin,
            destination,
            departure_date,
            candidates: candidates
                .iter()
                .filter(|(p, _)| *p == position)
                .map(|(_, c)| c.clone())
                .collect(),
        })
        .collect();

    Ok(Trip { id, name, adults, cabin_class, status, segments })
}
```

- [ ] **Step 6: Run the test**

Run: `cargo test --quiet a_trip_is_found_by_name`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/store.rs
git commit -m "feat: store named trips"
```

---

### Task 2: Segments — append, insert, drop

**Files:**
- Modify: `src/store.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn segments_stay_contiguous_through_inserts_and_drops() {
    // Positions are how the traveller refers to a segment ("drop the
    // second leg"), so a hole would make every later instruction target
    // the wrong row.
    let (store, _d) = test_store();
    let trip = store.upsert_trip(7, "September", None, None).unwrap();
    for (o, d, date) in [("AMS", "LIS", "2026-09-03"), ("LIS", "FCO", "2026-09-07")] {
        store.add_segment(trip.id, None, o, d, date).unwrap();
    }
    let trip = store.add_segment(trip.id, Some(2), "BCN", "MAD", "2026-09-05").unwrap();
    assert_eq!(
        trip.segments.iter().map(|s| (s.position, s.origin.as_str())).collect::<Vec<_>>(),
        vec![(1, "AMS"), (2, "BCN"), (3, "LIS")],
        "inserting at 2 shifts the rest down rather than colliding"
    );

    let trip = store.drop_segment(trip.id, 1).unwrap();
    assert_eq!(
        trip.segments.iter().map(|s| (s.position, s.origin.as_str())).collect::<Vec<_>>(),
        vec![(1, "BCN"), (2, "LIS")],
        "dropping the first renumbers what is left from 1"
    );

    // A position nobody has is refused rather than silently doing nothing.
    assert!(store.drop_segment(trip.id, 9).is_err());
}

#[test]
fn editing_a_trip_puts_it_back_to_planning() {
    let (store, _d) = test_store();
    let trip = store.upsert_trip(7, "September", None, None).unwrap();
    store.add_segment(trip.id, None, "AMS", "LIS", "2026-09-03").unwrap();
    store.set_trip_status(trip.id, "finalised").unwrap();
    assert_eq!(store.find_trip(7, "September").unwrap().unwrap().status, "finalised");

    let trip = store.add_segment(trip.id, None, "LIS", "AMS", "2026-09-10").unwrap();
    assert_eq!(trip.status, "planning", "the trip changed, so its pricing no longer describes it");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet segments_stay_contiguous`
Expected: FAIL to compile — `no method named add_segment`.

- [ ] **Step 3: Implement**

Add to `impl Store`:

```rust
/// Appends when `position` is None, otherwise inserts there and shifts the
/// rest down. Candidates move with their segment: they are keyed by
/// position, so a shift that forgot them would reattach somebody's chosen
/// flight to a different route.
pub fn add_segment(
    &self,
    trip_id: i64,
    position: Option<i64>,
    origin: &str,
    destination: &str,
    departure_date: &str,
) -> Result<Trip> {
    let conn = self.conn.lock().unwrap();
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM trip_segments WHERE trip_id = ?",
        params![trip_id],
        |row| row.get(0),
    )?;
    let at = match position {
        Some(p) if p >= 1 && p <= count => p,
        Some(p) if p == count + 1 => p,
        Some(p) => anyhow::bail!(
            "this trip has {count} segment(s), so position {p} is not somewhere to put one"
        ),
        None => count + 1,
    };
    if at <= count {
        // Descending is not needed: DuckDB applies this set-wise, so no
        // intermediate state can collide with the primary key.
        conn.execute(
            "UPDATE trip_segments SET position = position + 1
             WHERE trip_id = ? AND position >= ?",
            params![trip_id, at],
        )?;
        conn.execute(
            "UPDATE segment_candidates SET position = position + 1
             WHERE trip_id = ? AND position >= ?",
            params![trip_id, at],
        )?;
    }
    conn.execute(
        "INSERT INTO trip_segments (trip_id, position, origin, destination, departure_date)
         VALUES (?, ?, ?, ?, ?)",
        params![trip_id, at, origin, destination, departure_date],
    )?;
    touch(&conn, trip_id)?;
    load_trip(&conn, trip_id)
}

pub fn drop_segment(&self, trip_id: i64, position: i64) -> Result<Trip> {
    let conn = self.conn.lock().unwrap();
    let removed = conn.execute(
        "DELETE FROM trip_segments WHERE trip_id = ? AND position = ?",
        params![trip_id, position],
    )?;
    if removed == 0 {
        anyhow::bail!("this trip has no segment {position}");
    }
    conn.execute(
        "DELETE FROM segment_candidates WHERE trip_id = ? AND position = ?",
        params![trip_id, position],
    )?;
    // Closing the gap keeps positions contiguous, which is the invariant
    // that makes the shift above correct.
    conn.execute(
        "UPDATE trip_segments SET position = position - 1 WHERE trip_id = ? AND position > ?",
        params![trip_id, position],
    )?;
    conn.execute(
        "UPDATE segment_candidates SET position = position - 1
         WHERE trip_id = ? AND position > ?",
        params![trip_id, position],
    )?;
    touch(&conn, trip_id)?;
    load_trip(&conn, trip_id)
}

/// Used by finalisation to record that a trip has been priced.
pub fn set_trip_status(&self, trip_id: i64, status: &str) -> Result<()> {
    let conn = self.conn.lock().unwrap();
    conn.execute(
        "UPDATE trips SET status = ?, updated_at = current_timestamp WHERE id = ?",
        params![status, trip_id],
    )?;
    Ok(())
}
```

And the helper, next to `load_trip`:

```rust
/// Marks a trip edited. Status goes back to `planning` because whatever it
/// was priced at no longer describes it.
fn touch(conn: &Connection, trip_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE trips SET status = 'planning', updated_at = current_timestamp WHERE id = ?",
        params![trip_id],
    )?;
    Ok(())
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --quiet segments_stay_contiguous && cargo test --quiet editing_a_trip_puts_it_back`
Expected: both PASS.

- [ ] **Step 5: Commit**

```bash
git add src/store.rs
git commit -m "feat: add and drop trip segments, keeping positions contiguous"
```

---

### Task 3: Candidates — park, choose, drop

**Files:**
- Modify: `src/store.rs`

- [ ] **Step 1: Write the failing test**

```rust
fn candidate(airline: &str, numbers: &str, price: f64) -> NewCandidate {
    NewCandidate {
        airline: airline.to_string(),
        flight_numbers: numbers.to_string(),
        itinerary: format!("{numbers} somewhere"),
        departing_at_local: Some("2026-09-03T10:05:00".to_string()),
        arriving_at_local: Some("2026-09-03T12:15:00".to_string()),
        duration_minutes: Some(130),
        quoted_price: Some(price),
        quoted_currency: Some("EUR".to_string()),
        source: Some("duffel".to_string()),
    }
}

#[test]
fn a_segment_holds_several_options_and_at_most_one_is_chosen() {
    let (store, _d) = test_store();
    let trip = store.upsert_trip(7, "Japan", None, None).unwrap();
    let trip = store.add_segment(trip.id, None, "AMS", "NRT", "2026-09-03").unwrap();

    // Parked undecided: the traveller is comparing a nonstop against a
    // one-stop through Hong Kong.
    store.add_candidate(trip.id, 1, candidate("KLM", "KL861", 940.0), false).unwrap();
    let trip = store.add_candidate(trip.id, 1, candidate("Cathay", "CX270,CX500", 780.0), false).unwrap();
    let options = &trip.segments[0].candidates;
    assert_eq!(options.len(), 2);
    assert_eq!(options.iter().map(|c| c.candidate).collect::<Vec<_>>(), vec![1, 2]);
    assert!(options.iter().all(|c| !c.chosen), "nothing decided yet");

    let trip = store.choose_candidate(trip.id, 1, 2).unwrap();
    let options = &trip.segments[0].candidates;
    assert!(!options[0].chosen);
    assert!(options[1].chosen);

    // Choosing again moves the flag rather than setting a second one.
    let trip = store.choose_candidate(trip.id, 1, 1).unwrap();
    let options = &trip.segments[0].candidates;
    assert_eq!(options.iter().filter(|c| c.chosen).count(), 1);
    assert!(options[0].chosen);

    // A candidate the segment does not have.
    assert!(store.choose_candidate(trip.id, 1, 9).is_err());

    let trip = store.drop_candidate(trip.id, 1, 1).unwrap();
    assert_eq!(trip.segments[0].candidates.len(), 1, "the segment survives losing an option");
    assert_eq!(trip.segments[0].candidates[0].candidate, 2, "numbers are not reused");
}

#[test]
fn adding_a_decided_option_marks_it_chosen_in_one_call() {
    // The ordinary path — "book me on this one" — must not need a second
    // call to say what it obviously meant.
    let (store, _d) = test_store();
    let trip = store.upsert_trip(7, "Japan", None, None).unwrap();
    let trip = store.add_segment(trip.id, None, "AMS", "NRT", "2026-09-03").unwrap();
    let trip = store.add_candidate(trip.id, 1, candidate("KLM", "KL861", 940.0), true).unwrap();
    assert!(trip.segments[0].candidates[0].chosen);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet a_segment_holds_several_options`
Expected: FAIL to compile — `no method named add_candidate`.

- [ ] **Step 3: Implement**

Add to `impl Store`:

```rust
/// Parks a flight against a segment. `decided` also marks it chosen, so
/// the common single-option path is one call.
///
/// Candidate numbers are never reused: they are what the traveller sees and
/// what `choose_candidate` takes, and recycling one would silently retarget
/// a decision made against the old numbering.
pub fn add_candidate(
    &self,
    trip_id: i64,
    position: i64,
    new: NewCandidate,
    decided: bool,
) -> Result<Trip> {
    let conn = self.conn.lock().unwrap();
    let exists: i64 = conn.query_row(
        "SELECT count(*) FROM trip_segments WHERE trip_id = ? AND position = ?",
        params![trip_id, position],
        |row| row.get(0),
    )?;
    if exists == 0 {
        anyhow::bail!("this trip has no segment {position}");
    }
    let next: i64 = conn.query_row(
        "SELECT coalesce(max(candidate), 0) + 1 FROM segment_candidates
         WHERE trip_id = ? AND position = ?",
        params![trip_id, position],
        |row| row.get(0),
    )?;
    conn.execute(
        "INSERT INTO segment_candidates (
             trip_id, position, candidate, chosen, airline, flight_numbers, itinerary,
             departing_at_local, arriving_at_local, duration_minutes,
             quoted_price, quoted_currency, quoted_at, source)
         VALUES (?, ?, ?, false, ?, ?, ?, ?, ?, ?, ?, ?, current_timestamp, ?)",
        params![
            trip_id,
            position,
            next,
            new.airline,
            new.flight_numbers,
            new.itinerary,
            new.departing_at_local,
            new.arriving_at_local,
            new.duration_minutes,
            new.quoted_price,
            new.quoted_currency,
            new.source
        ],
    )?;
    if decided {
        choose_within(&conn, trip_id, position, next)?;
    }
    touch(&conn, trip_id)?;
    load_trip(&conn, trip_id)
}

pub fn choose_candidate(&self, trip_id: i64, position: i64, candidate: i64) -> Result<Trip> {
    let conn = self.conn.lock().unwrap();
    choose_within(&conn, trip_id, position, candidate)?;
    touch(&conn, trip_id)?;
    load_trip(&conn, trip_id)
}

pub fn drop_candidate(&self, trip_id: i64, position: i64, candidate: i64) -> Result<Trip> {
    let conn = self.conn.lock().unwrap();
    let removed = conn.execute(
        "DELETE FROM segment_candidates WHERE trip_id = ? AND position = ? AND candidate = ?",
        params![trip_id, position, candidate],
    )?;
    if removed == 0 {
        anyhow::bail!("segment {position} has no option {candidate}");
    }
    touch(&conn, trip_id)?;
    load_trip(&conn, trip_id)
}
```

And the helper next to `touch`:

```rust
/// Clears the position's flags and sets one. "At most one chosen" cannot be
/// a `UNIQUE` constraint because `false` repeats, so it is this function's
/// job — and the caller always holds the connection lock, which is what
/// makes the pair of statements indivisible.
fn choose_within(conn: &Connection, trip_id: i64, position: i64, candidate: i64) -> Result<()> {
    let known: i64 = conn.query_row(
        "SELECT count(*) FROM segment_candidates
         WHERE trip_id = ? AND position = ? AND candidate = ?",
        params![trip_id, position, candidate],
        |row| row.get(0),
    )?;
    if known == 0 {
        anyhow::bail!("segment {position} has no option {candidate}");
    }
    conn.execute(
        "UPDATE segment_candidates SET chosen = false WHERE trip_id = ? AND position = ?",
        params![trip_id, position],
    )?;
    conn.execute(
        "UPDATE segment_candidates SET chosen = true
         WHERE trip_id = ? AND position = ? AND candidate = ?",
        params![trip_id, position, candidate],
    )?;
    Ok(())
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --quiet a_segment_holds_several_options && cargo test --quiet adding_a_decided_option`
Expected: both PASS.

- [ ] **Step 5: Commit**

```bash
git add src/store.rs
git commit -m "feat: park several flight options on a trip segment"
```

---

### Task 4: Deleting a trip

**Files:**
- Modify: `src/store.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn deleting_a_trip_takes_its_segments_and_options_with_it() {
    // Creating a trip is a side effect of a typo, so a typo needs an undo.
    let (store, _d) = test_store();
    let trip = store.upsert_trip(7, "Setpember", None, None).unwrap();
    let trip = store.add_segment(trip.id, None, "AMS", "LIS", "2026-09-03").unwrap();
    store.add_candidate(trip.id, 1, candidate("TAP", "TP675", 118.0), true).unwrap();

    assert!(store.delete_trip(7, "setpember").unwrap());
    assert!(store.find_trip(7, "Setpember").unwrap().is_none());
    assert!(!store.delete_trip(7, "Setpember").unwrap(), "deleting twice is not an error");

    // Another user's trip of the same name is untouched.
    store.upsert_trip(8, "Setpember", None, None).unwrap();
    assert!(!store.delete_trip(7, "Setpember").unwrap());
    assert!(store.find_trip(8, "Setpember").unwrap().is_some());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet deleting_a_trip_takes_its_segments`
Expected: FAIL to compile — `no method named delete_trip`.

- [ ] **Step 3: Implement**

```rust
/// False when there was no such trip. Deleting something already gone is
/// the state the caller wanted, not a failure.
pub fn delete_trip(&self, user_id: i64, name: &str) -> Result<bool> {
    let key = name.trim().to_lowercase();
    let conn = self.conn.lock().unwrap();
    let mut stmt =
        conn.prepare("SELECT id FROM trips WHERE user_id = ? AND name_key = ?")?;
    let mut ids = stmt.query_map(params![user_id, key], |row| row.get::<_, i64>(0))?;
    let Some(id) = ids.next().transpose()? else {
        return Ok(false);
    };
    drop(ids);
    drop(stmt);
    conn.execute("DELETE FROM segment_candidates WHERE trip_id = ?", params![id])?;
    conn.execute("DELETE FROM trip_segments WHERE trip_id = ?", params![id])?;
    conn.execute("DELETE FROM trips WHERE id = ?", params![id])?;
    Ok(true)
}
```

- [ ] **Step 4: Run the test**

Run: `cargo test --quiet deleting_a_trip_takes_its_segments`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/store.rs
git commit -m "feat: delete a trip and everything hanging off it"
```

---

### Task 5: The itinerary checks

**Files:**
- Create: `src/tools/trips.rs`
- Modify: `src/tools/mod.rs`

These are reported, never refused — a gap between segments is a train ride, not a mistake.

- [ ] **Step 1: Write the failing test**

Create `src/tools/trips.rs` with only this at first:

```rust
//! Building a trip: a named plan the traveller assembles over as many
//! messages as it takes.
//!
//! Nothing here stores an offer id. Offers expire in minutes and a plan
//! outlives the conversation that made it, so a trip holds the itinerary —
//! airports, dates, flight numbers — and finalisation re-prices it.

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
```

- [ ] **Step 2: Wire the module and run the test**

Add to `src/tools/mod.rs`:

```rust
pub mod trips;
```

Run: `cargo test --quiet a_gap_between_segments`
Expected: FAIL to compile — `cannot find function itinerary_notes`.

- [ ] **Step 3: Implement**

Add above the test module in `src/tools/trips.rs`:

```rust
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
            if (0..TIGHT_TURNAROUND_MINUTES).contains(&minutes) {
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
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --quiet trips::`
Expected: 5 passed.

- [ ] **Step 5: Commit**

```bash
git add src/tools/trips.rs src/tools/mod.rs
git commit -m "feat: notice gaps and backwards dates in a trip"
```

---

### Task 6: `add_trip_segment`

**Files:**
- Modify: `src/tools/trips.rs`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/tools/trips.rs`:

```rust
use crate::store::Store;
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet adding_a_segment_creates_the_trip`
Expected: FAIL to compile — `cannot find struct AddTripSegmentTool`.

- [ ] **Step 3: Implement**

Add to `src/tools/trips.rs`:

```rust
use crate::store::{Store, Trip};
use crate::tools::purchases::{internal, StoreToolError};
use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

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
            "{label} must be a 3-letter IATA airport or city code (AMS, LHR, NYC), not {value:?}"
        ))),
    }
}

fn calendar_date(value: &str) -> Result<String, StoreToolError> {
    let date = value.trim();
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_err(|_| StoreToolError(format!("departure_date must be YYYY-MM-DD, not {value:?}")))?;
    Ok(date.to_string())
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
                "adults": {"type": "integer", "description": "passengers for the whole trip, default 1"},
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
            store.add_segment(trip.id, args.position, &origin, &destination, &date)
        })
        .await
        .map_err(internal)?
        .map(TripView::of)
        .map_err(internal)
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --quiet trips::`
Expected: 7 passed.

- [ ] **Step 5: Commit**

```bash
git add src/tools/trips.rs
git commit -m "feat: add_trip_segment tool"
```

---

### Task 7: `add_trip_option` and `choose_trip_option`

**Files:**
- Modify: `src/tools/trips.rs`

The guard here is the one `BookingLinksTool` already uses: a flight must be one this chat was actually shown. A model retyping a 32-character id across a turn is the invented-ASIN problem in different clothing.

- [ ] **Step 1: Write the failing test**

```rust
use crate::tools::duffel::{Flight, Leg, PriceStatus, Source};
use crate::tools::shown::ShownFlights;
use std::sync::Arc;
use std::time::Instant;

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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet two_options_can_sit_on_one_segment`
Expected: FAIL to compile — `cannot find struct AddTripOptionTool`.

- [ ] **Step 3: Implement**

```rust
use crate::store::NewCandidate;
use crate::tools::duffel::{Flight, Source};
use crate::tools::shown::ShownFlights;
use std::sync::Arc;

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
        let flight = self.shown.find(self.chat_id, &args.offer_id, Instant::now()).ok_or_else(
            || {
                let ids = self.shown.offer_ids(self.chat_id, Instant::now());
                StoreToolError(format!(
                    "{:?} was not shown in this conversation, so it cannot be added. \
                     Search first, then use one of these offer_ids: {}",
                    args.offer_id,
                    match ids.is_empty() {
                        true => "(nothing has been searched yet)".to_string(),
                        false => ids.join(", "),
                    }
                ))
            },
        )?;

        if flight.legs.len() != 1 {
            return Err(StoreToolError(format!(
                "that offer is a return with {} legs, and a trip segment is one direction. \
                 Add each direction as its own segment and pick a one-way for each.",
                flight.legs.len()
            )));
        }
        let leg = &flight.legs[0];
        let candidate = candidate_from(&flight);

        let store = self.store.clone();
        let user_id = self.user_id;
        let (leg_origin, leg_destination) = (leg.origin.clone(), leg.destination.clone());
        tokio::task::spawn_blocking(move || -> anyhow::Result<Trip> {
            let trip = store
                .find_trip(user_id, &args.trip)?
                .ok_or_else(|| anyhow::anyhow!("no trip called {:?}", args.trip))?;
            let segment = trip
                .segments
                .iter()
                .find(|s| s.position == args.position)
                .ok_or_else(|| anyhow::anyhow!("this trip has no segment {}", args.position))?;
            // Binding an AMS→LIS flight to an AMS→NRT segment would look
            // fine in the reply and be re-priced against the wrong route.
            if segment.origin != leg_origin || segment.destination != leg_destination {
                anyhow::bail!(
                    "segment {} is {}→{} but that flight is {leg_origin}→{leg_destination}",
                    segment.position,
                    segment.origin,
                    segment.destination
                );
            }
            store.add_candidate(trip.id, args.position, candidate, args.decided.unwrap_or(true))
        })
        .await
        .map_err(internal)?
        .map(TripView::of)
        .map_err(internal)
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
            let trip = store
                .find_trip(user_id, &args.trip)?
                .ok_or_else(|| anyhow::anyhow!("no trip called {:?}", args.trip))?;
            store.choose_candidate(trip.id, args.position, args.candidate)
        })
        .await
        .map_err(internal)?
        .map(TripView::of)
        .map_err(internal)
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --quiet trips::`
Expected: 11 passed.

- [ ] **Step 5: Commit**

```bash
git add src/tools/trips.rs
git commit -m "feat: park and choose flight options on a trip segment"
```

---

### Task 8: `show_trip`, `drop_trip_segment`, `delete_trip`

**Files:**
- Modify: `src/tools/trips.rs`

- [ ] **Step 1: Write the failing test**

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet show_lists_every_trip`
Expected: FAIL to compile — `cannot find struct ShowTripTool`.

- [ ] **Step 3: Implement**

```rust
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
                Some(name) => match store.find_trip(user_id, name)? {
                    Some(trip) => Ok(vec![trip]),
                    // Naming the real trips is the difference between the
                    // model correcting itself and it inventing a new one.
                    None => {
                        let names: Vec<String> =
                            store.list_trips(user_id)?.into_iter().map(|t| t.name).collect();
                        anyhow::bail!(
                            "no trip called {name:?}. This traveller has: {}",
                            match names.is_empty() {
                                true => "none yet".to_string(),
                                false => names.join(", "),
                            }
                        )
                    }
                },
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
            let trip = store
                .find_trip(user_id, &args.trip)?
                .ok_or_else(|| anyhow::anyhow!("no trip called {:?}", args.trip))?;
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
        tokio::task::spawn_blocking(move || store.delete_trip(user_id, &args.trip))
            .await
            .map_err(internal)?
            .map(|deleted| DeleteTripResult { deleted, trip: name })
            .map_err(internal)
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --quiet trips::`
Expected: 14 passed.

- [ ] **Step 5: Commit**

```bash
git add src/tools/trips.rs
git commit -m "feat: show, drop and delete trips"
```

---

### Task 9: Register the tools

**Files:**
- Modify: `src/agent.rs:580-586` (the unconditional tool chain) and the preamble

- [ ] **Step 1: Register**

In `src/agent.rs`, add to the `use` block at the top:

```rust
use crate::tools::trips::{
    AddTripOptionTool, AddTripSegmentTool, ChooseTripOptionTool, DeleteTripTool,
    DropTripSegmentTool, ShowTripTool,
};
```

Then extend the builder chain that currently ends with `.tool(ForgetFactTool { store: d.store.clone(), user_id });` — change that line's terminating semicolon into a continuation and append:

```rust
        .tool(ForgetFactTool { store: d.store.clone(), user_id })
        // Trip planning. Registered unconditionally: a trip is a plan, and
        // planning one needs no provider at all. Only finalising does.
        .tool(AddTripSegmentTool { store: d.store.clone(), user_id })
        .tool(AddTripOptionTool {
            store: d.store.clone(),
            user_id,
            shown: d.shown.clone(),
            chat_id,
        })
        .tool(ChooseTripOptionTool { store: d.store.clone(), user_id })
        .tool(ShowTripTool { store: d.store.clone(), user_id })
        .tool(DropTripSegmentTool { store: d.store.clone(), user_id })
        .tool(DeleteTripTool { store: d.store.clone(), user_id });
```

- [ ] **Step 2: Add preamble guidance**

Find the preamble string in `preamble_with_profile` and add this paragraph to the flights section:

```
When someone is planning more than one flight — a multi-city route, or a trip
they are assembling over several messages — build it with the trip tools
rather than holding it in your head. A segment is one direction on one date,
so a return is two segments. If they are undecided between flights, park each
with add_trip_option and decided=false rather than picking for them; several
options may sit on one segment and cost nothing extra. Quote a trip's prices
as of when each option was parked, never as a current total: they are stale
by construction and only finalising re-prices them.
```

- [ ] **Step 3: Verify the whole suite and lints**

Run: `cargo test --quiet && cargo clippy --all-targets --quiet`
Expected: all tests pass; clippy silent.

Note: this repo is **not** `cargo fmt`-formatted (every file differs from rustfmt defaults and there is no `rustfmt.toml`). Do not run `cargo fmt` — match the surrounding style by hand.

- [ ] **Step 4: Commit**

```bash
git add src/agent.rs
git commit -m "feat: give the agent the trip planning tools"
```

---

## Done when

- `cargo test` passes with the new tests included
- `cargo clippy --all-targets` is silent
- A trip can be created, given segments, given several options per segment, have one chosen, be shown, amended and deleted — all from tool calls
- No offer id is stored anywhere in the schema
