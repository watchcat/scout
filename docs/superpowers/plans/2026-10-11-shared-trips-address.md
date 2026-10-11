# Shared Trips, Slice 2 — The Trip's Address: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every kept trip has its own address, `hong-kong-sep-4f9qa@goodscout.fyi`; mail sent to it lands on that trip for everyone on it, is forwarded to each of them, and its Add and Ignore buttons go to each of them on Telegram, the first press deciding for all.

**Architecture:** A trip address always contains a dash and a personal handle never can, so the two never collide and need no shared uniqueness check. Inbound mail that matches no handle is looked up among trip addresses and stored under the trip's owner with `inbound_mail.trip_id` set. One SQL predicate, `ARRIVAL_REACH`, widens every arrival and mail query from "this account's" to "this account's, or mail to a trip this account reaches", so the page, the Add and Ignore routes and the Telegram buttons work for an editor unchanged. The worker forwards and nudges once per person, and a table records who has had their forward so a retry sends nobody a second copy.

**Tech Stack:** Rust (axum 0.8, DuckDB through `duckdb-rs` without `chrono`, teloxide), plain JS (`chat.js`, node `--test`).

**Spec:** `docs/superpowers/specs/2026-10-11-shared-trips-design.md`, Slice 2. Slice 1's plan is `2026-10-11-shared-trips-people.md`; its helpers (`REACHES`, `reachable_trip_id`, `trip_people`, `author_names`, `Plan::for_reader`, `sharing::share_for_tests`) are used here.

**Gate (before every commit that touches code):**

```bash
cargo test --workspace 2>&1 | grep -E "^test result|FAILED|panicked"
cargo clippy --workspace --all-targets -- -D warnings
node --test 'crates/scout-web/src/*.test.mjs'
```

Never run `cargo fmt`. Commit trailer: `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Branch `feat/trip-address`.

## Decisions made while planning

Task 0 writes these into the spec.

- **The two kinds of address cannot collide.** A handle is `a-z0-9.` only (`normalise_handle`); a trip address is a slug, a dash and a tail, so it always has a dash. The spec's shared uniqueness check, and "setting a personal handle checks trip addresses too", are unnecessary. Trip addresses are unique among themselves, checked under the store lock.
- **The tail is 5 characters of `a-z0-9`**, from `rand`, which `scout-core` gains as a dependency (already in the lockfile through `scout-web`).
- **`arrivals.trip_id` already exists**: it is where placement puts a reading. Step 22 adds `inbound_mail.trip_id` (the mail came to a trip's address), `arrivals.settled_by`, `trips.address` and `mail_forwards`.
- **An address is given when a kept trip is first served** (`Plan::for_reader`), not when it is kept. One code path covers trips kept by the page, by Scout, by an Add and by sharing, and trips kept before the release.
- **Anyone on the trip may delete one of its mails under Other mail**, as anyone may Add or Ignore its bookings.
- **A slice-1 bug fixed here:** `ARRIVAL_SELECT` joins the arrival's trip on `t.account_id = a.account_id`, so a booking placed on a trip shared with the account shows no trip name. The join becomes `t.id = a.trip_id`.

## File map

| File | What changes |
|---|---|
| `crates/scout-core/Cargo.toml` | `rand = "0.8"` |
| `crates/scout-core/src/store.rs` | step 22; trip addresses; `insert_trip_mail`; `MailToWork.trip_id`; `ARRIVAL_REACH` through every arrival and mail query; `settled_by`; `mail_forwards` |
| `crates/scout-core/src/inbox.rs` | `trip_address_slug`, `normalise_trip_address`, `trip_for_address`, `record_trip_mail`; `record_arrivals` with a fixed trip; `settler` |
| `crates/scout-core/src/trips.rs` | `Plan.address` |
| `crates/scout-core/src/sharing.rs` | `new_address` |
| `crates/scout-web/src/inbound.rs` | handle first, then trip address |
| `crates/scout-web/src/inbox_worker.rs` | forward and nudge per person on a trip's mail |
| `crates/scout-telegram/src/bot.rs` | a late press says who decided |
| `crates/scout-web/src/routes/trips.rs` | `POST /chat/trips/address-new` |
| `crates/scout-web/src/chat.js`, `chat.test.mjs`, `chat.html` | the address in the People panel and the to-book banner |

---

### Task 0: Branch and spec amendments

- [ ] **Step 1:** `git checkout -b feat/trip-address`
- [ ] **Step 2:** In the spec's Slice 2 "Data", replace the SQL with:

```sql
ALTER TABLE trips        ADD COLUMN IF NOT EXISTS address    TEXT;
ALTER TABLE inbound_mail ADD COLUMN IF NOT EXISTS trip_id    BIGINT;
ALTER TABLE arrivals     ADD COLUMN IF NOT EXISTS settled_by BIGINT;
CREATE TABLE IF NOT EXISTS mail_forwards (
    mail_id      BIGINT NOT NULL,
    account_id   BIGINT NOT NULL,
    forwarded_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (mail_id, account_id)
);
```

and replace the bullet beginning "`trips.address` is the local part" with:

```markdown
- `trips.address` is the local part, lowercase. It always contains a
  dash, which a personal handle (`a-z0-9.`) never can, so the two kinds
  never collide; trip addresses are unique among themselves, checked in
  Rust under the store lock.
- `arrivals.trip_id` already exists — placement writes it — and a trip's
  mail sets it to that trip.
```

In "### The address", change "4 random characters" to "5 random characters", and replace "Made when a trip is kept" with "Given to a kept trip the first time it is served, which covers trips kept before this release too". Add under "### Mail in":

```markdown
Anyone on the trip may delete one of its mails under Other mail, as
anyone may Add or Ignore its bookings.
```

- [ ] **Step 3:** Commit `docs: trip address plan, and what planning changed in the spec`.

---

### Task 1: Schema step 22

**Files:** `crates/scout-core/src/store.rs`

- [ ] **Step 1: Write the failing test** (beside slice 1's `step_21_runs_on…`)

```rust
    /// A database at 21 with a trip, a mail and an arrival in it, stripped
    /// of what step 22 adds.
    fn version_twenty_one_db() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("v21.duckdb");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATIONS).unwrap();
        conn.execute_batch(
            "ALTER TABLE trips DROP COLUMN address;
             ALTER TABLE inbound_mail DROP COLUMN trip_id;
             ALTER TABLE arrivals DROP COLUMN settled_by;
             DROP TABLE mail_forwards;
             CREATE TABLE IF NOT EXISTS schema_version (version BIGINT NOT NULL);
             DELETE FROM schema_version;
             INSERT INTO schema_version VALUES (21);
             INSERT INTO accounts (id) VALUES (1);
             INSERT INTO trips (id, account_id, name, name_key, kept) VALUES (1, 1, 'Lisbon', 'lisbon', true);
             INSERT INTO inbound_mail (id, account_id, provider_id, from_address) VALUES (1, 1, 'p1', 'hotel@example.com');
             INSERT INTO arrivals (id, account_id, mail_id, booking, summary, trip_id) VALUES (1, 1, 1, true, 'Hotel', 1);",
        )
        .unwrap();
        drop(conn);
        (dir, path)
    }

    #[test]
    fn step_22_runs_on_a_database_with_mail_in_it_and_matches_a_fresh_one() {
        let (_dir, path) = version_twenty_one_db();
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), 22);
        let (fresh, _d) = test_store();
        for table in ["trips", "inbound_mail", "arrivals", "mail_forwards"] {
            assert_eq!(shape(&fresh.conn(), table), shape(&s.conn(), table), "{table}");
        }
    }
```

- [ ] **Step 2:** Run `cargo test -p scout-core step_22`; expected FAIL (no `address` column to drop).
- [ ] **Step 3: `MIGRATIONS`.** In `trips`, after `owner_name  TEXT,`:

```sql
    -- The trip's own address, the local part. Step 22; NULL until the
    -- trip is kept and served. Always holds a dash, which a handle cannot.
    address     TEXT,
```

In `inbound_mail`, after `error        TEXT` (add a comma):

```sql
    -- Set when the mail came to a trip's address rather than a person's;
    -- `account_id` is then the trip's owner. Step 22.
    trip_id      BIGINT
```

In `arrivals`, after `stops             TEXT` (add a comma):

```sql
    -- Who pressed Add or Ignore, on a trip several people share. Step 22.
    settled_by        BIGINT
```

After `trip_links`:

```sql
-- Who a trip's mail has been forwarded to, one row each, so a retry after
-- a partial failure sends nobody a second copy.
CREATE TABLE IF NOT EXISTS mail_forwards (
    mail_id      BIGINT NOT NULL,
    account_id   BIGINT NOT NULL,
    forwarded_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (mail_id, account_id)
);
```

- [ ] **Step 4: The step**

```rust
/// A trip's own address, and its mail: where it came to, who settled its
/// bookings, who it was forwarded to. Nullable columns and a new table,
/// as step 21.
const STEP_22_TRIP_ADDRESS: &str = r#"
ALTER TABLE trips ADD COLUMN IF NOT EXISTS address TEXT;
ALTER TABLE inbound_mail ADD COLUMN IF NOT EXISTS trip_id BIGINT;
ALTER TABLE arrivals ADD COLUMN IF NOT EXISTS settled_by BIGINT;
CREATE TABLE IF NOT EXISTS mail_forwards (
    mail_id BIGINT NOT NULL, account_id BIGINT NOT NULL,
    forwarded_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (mail_id, account_id)
);
"#;
```

`(22, Step::Sql(STEP_22_TRIP_ADDRESS)),` in `steps()`. Move every "latest" assertion: `sed -i '' 's/schema_version().unwrap(), 21\([,)]\)/schema_version().unwrap(), 22\1/g' crates/scout-core/src/store.rs`. Each older fixture that compares shapes must also drop step 22's columns from the tables it compares: in `version_twenty_db_with_an_item` add `ALTER TABLE trips DROP COLUMN address;`; check `version_sixteen_db` (it compares `arrivals`) and add `ALTER TABLE arrivals DROP COLUMN settled_by;`, and any fixture that compares `inbound_mail`, `ALTER TABLE inbound_mail DROP COLUMN trip_id;`.

- [ ] **Step 5:** Run `cargo test -p scout-core store::` → PASS. Commit `feat(store): step 22, a trip's address and its mail`.

---

### Task 2: Trip addresses in the store

**Files:** `crates/scout-core/Cargo.toml`, `crates/scout-core/src/inbox.rs`, `crates/scout-core/src/store.rs`

- [ ] **Step 1: Write the failing tests.** In `inbox.rs` tests:

```rust
    #[test]
    fn a_trip_address_is_the_name_as_a_slug_with_a_tail() {
        assert_eq!(trip_address_slug("Hong Kong, September!"), "hong-kong-september");
        assert_eq!(trip_address_slug("  Lisbon  "), "lisbon");
        assert_eq!(trip_address_slug("A very long trip name that goes on"), "a-very-long-trip-nam");
        assert_eq!(trip_address_slug("Москва"), "trip");
        assert_eq!(trip_address_slug("---"), "trip");
        assert_eq!(normalise_trip_address(" Hong-Kong-SEP-4f9qa "), Some("hong-kong-sep-4f9qa".into()));
        assert_eq!(normalise_trip_address("sasha"), None, "no dash: a handle's shape, not a trip's");
        assert_eq!(normalise_trip_address("a-b.c"), None);
        assert_eq!(normalise_trip_address("-lisbon-"), None);
    }
```

In `store.rs` tests:

```rust
    #[test]
    fn a_kept_trip_gets_one_address_and_the_owner_can_replace_it() {
        let (s, _d) = test_store();
        let owner = s.account_for_telegram(1).unwrap();
        let trip = s.upsert_trip(owner, "Lisbon", None, None, None).unwrap();
        assert_eq!(s.trip_address(trip.id).unwrap(), None, "a draft has none");
        s.keep_trip(owner, "Lisbon").unwrap();
        let first = s.trip_address(trip.id).unwrap().unwrap();
        assert!(first.starts_with("lisbon-") && first.len() == "lisbon-".len() + 5, "{first}");
        assert_eq!(s.trip_address(trip.id).unwrap().as_deref(), Some(first.as_str()), "the same one next time");
        assert_eq!(s.trip_for_address(&first).unwrap(), Some((trip.id, owner)));
        let editor = s.account_for_telegram(2).unwrap();
        put_on(&s, trip.id, editor, "Masha");
        assert_eq!(s.new_trip_address(editor, trip.id).unwrap(), None, "the owner's to replace");
        let second = s.new_trip_address(owner, trip.id).unwrap().unwrap();
        assert_ne!(first, second);
        assert_eq!(s.trip_for_address(&first).unwrap(), None, "the old one stops working");
        assert_eq!(s.trip_for_address(&second).unwrap(), Some((trip.id, owner)));
    }
```

- [ ] **Step 2:** Run; expected compile errors.
- [ ] **Step 3: Implement.** `Cargo.toml`: add `rand = "0.8"` under `[dependencies]`.

In `inbox.rs`, beside `normalise_handle`:

```rust
/// The readable half of a trip's address: the name lowercased, anything
/// outside `a-z0-9` turned into one dash, trimmed, cut to 20 characters.
/// "trip" when nothing is left — a name in another script has no letters
/// an address can carry.
pub fn trip_address_slug(name: &str) -> String {
    let mut slug = String::new();
    for c in name.trim().to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(20).collect();
    let slug = slug.trim_end_matches('-').to_string();
    if slug.is_empty() { "trip".to_string() } else { slug }
}

/// A local part read as a trip address: `a-z0-9` and dashes, at least one
/// dash, none at either end. A handle never has a dash, so the two kinds
/// of address can be told apart by shape alone.
pub fn normalise_trip_address(raw: &str) -> Option<String> {
    let a = raw.trim().to_ascii_lowercase();
    let ok = a.contains('-')
        && !a.starts_with('-')
        && !a.ends_with('-')
        && a.len() <= 40
        && a.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    ok.then_some(a)
}
```

In `store.rs`:

```rust
/// Five characters of `a-z0-9`: the part of a trip's address nobody can
/// guess from the trip's name.
fn address_tail() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..5).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect()
}

/// Draws a fresh address for the trip and writes it, under the lock the
/// caller holds. Ten tries at a tail no other trip has; the eleventh
/// collision in a space of sixty million is a bug, said as one.
fn draw_address(conn: &Connection, trip_id: i64, name: &str) -> Result<String> {
    let slug = crate::inbox::trip_address_slug(name);
    for _ in 0..10 {
        let candidate = format!("{slug}-{}", address_tail());
        let taken: i64 =
            conn.query_row("SELECT count(*) FROM trips WHERE address = ?", params![candidate], |r| r.get(0))?;
        if taken == 0 {
            conn.execute("UPDATE trips SET address = ? WHERE id = ?", params![candidate, trip_id])?;
            return Ok(candidate);
        }
    }
    anyhow::bail!("no free address for trip {trip_id} after ten draws")
}
```

and in `impl Store`:

```rust
    /// The trip's address, given now if it is kept and has none. `None` for
    /// a draft: an address on a trip that may be gone tomorrow would send a
    /// hotel's confirmation nowhere.
    pub fn trip_address(&self, trip_id: i64) -> Result<Option<String>> {
        let conn = self.conn();
        let (address, kept, name): (Option<String>, bool, String) = conn.query_row(
            "SELECT address, kept, name FROM trips WHERE id = ?",
            params![trip_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        match (address, kept) {
            (Some(address), _) => Ok(Some(address)),
            (None, false) => Ok(None),
            (None, true) => draw_address(&conn, trip_id, &name).map(Some),
        }
    }

    /// A new address in place of the old, for the owner; the old one then
    /// belongs to nobody. `None` for anyone else.
    pub fn new_trip_address(&self, account_id: i64, trip_id: i64) -> Result<Option<String>> {
        if !self.is_owner(account_id, trip_id)? {
            return Ok(None);
        }
        let conn = self.conn();
        let name: String = conn.query_row("SELECT name FROM trips WHERE id = ?", params![trip_id], |r| r.get(0))?;
        draw_address(&conn, trip_id, &name).map(Some)
    }

    /// Whose trip an address is: the trip and its owner.
    pub fn trip_for_address(&self, address: &str) -> Result<Option<(i64, i64)>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT id, account_id FROM trips WHERE address = ?",
                params![address],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }
```

- [ ] **Step 4:** Run `cargo test -p scout-core address` → PASS. Commit `feat(store): a kept trip has an address of its own`.

---

### Task 3: Arrivals and mail reach everyone on the trip

**Files:** `crates/scout-core/src/store.rs` (`ARRIVAL_SELECT` ~5357, `insert_mail` ~5497, `mail_to_work` ~5579, `pending_arrivals_of_mail`, `arrival_of`, `decide_arrival`, `inbox_view`, `delete_mail` ~6048, `sweep_inbox`, `MailToWork` ~1656)

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn a_trips_mail_and_its_bookings_reach_everyone_on_the_trip_and_record_who_settled_them() {
        let (s, _d) = test_store();
        let owner = s.account_for_telegram(1).unwrap();
        let (editor, stranger) = (s.account_for_telegram(2).unwrap(), s.account_for_telegram(3).unwrap());
        let trip = s.upsert_trip(owner, "Lisbon", None, None, None).unwrap();
        put_on(&s, trip.id, editor, "Masha");
        let mail = s
            .insert_trip_mail(trip.id, owner, "p-1", "hotel@example.com", Some("Your stay"), None, None, false, &[])
            .unwrap()
            .unwrap();
        assert_eq!(s.mail_to_work(10).unwrap()[0].trip_id, Some(trip.id));
        let hotel = NewArrival { booking: true, summary: "Hotel".into(), trip_id: Some(trip.id), ..NewArrival::default() };
        let not_one = NewArrival { booking: false, summary: "Newsletter".into(), ..NewArrival::default() };
        let ids = s.insert_arrivals(owner, mail, &[hotel, not_one]).unwrap();

        assert_eq!(s.pending_arrivals_of_mail(mail, editor).unwrap().len(), 1);
        assert!(s.arrival_of(ids[0], editor).unwrap().is_some());
        assert_eq!(s.arrival_of(ids[0], editor).unwrap().unwrap().trip_name.as_deref(), Some("Lisbon"));
        assert!(s.arrival_of(ids[0], stranger).unwrap().is_none());
        assert_eq!(s.inbox_view(editor).unwrap().pending.len(), 1);
        assert_eq!(s.inbox_view(editor).unwrap().other.len(), 1, "the newsletter, under Other mail");
        assert!(s.inbox_view(stranger).unwrap().pending.is_empty());

        assert!(s.decide_arrival(ids[0], editor, "added", None).unwrap());
        assert!(!s.decide_arrival(ids[0], owner, "ignored", None).unwrap(), "the first press decided");
        assert_eq!(s.arrival_settler(ids[0]).unwrap(), Some(("added".to_string(), editor)));

        assert!(s.forwarded_to(mail).unwrap().is_empty());
        s.note_forwarded_to(mail, editor).unwrap();
        s.note_forwarded_to(mail, editor).unwrap();
        assert_eq!(s.forwarded_to(mail).unwrap(), vec![editor]);
    }
```

`NewArrival` needs `Default` for this; derive it if it does not have it (`#[derive(Default)]` — every field is an `Option`, a `bool` or a `String`, except any that is not, which the derive will name).

- [ ] **Step 2:** Run; expected compile errors.
- [ ] **Step 3: The predicate**

Beside `ARRIVAL_SELECT`:

```rust
/// True when the account bound to the three `?` may see and decide the
/// arrival `a` of mail `m`: it is the account's own, or it came to the
/// address of a trip the account reaches. `REACHES` with `t` renamed,
/// since `ARRIVAL_SELECT` already uses `t` for the arrival's trip.
const ARRIVAL_REACH: &str = "(a.account_id = ? OR (m.trip_id IS NOT NULL AND EXISTS (
     SELECT 1 FROM trips mt WHERE mt.id = m.trip_id AND (mt.account_id = ? OR EXISTS (
         SELECT 1 FROM trip_people p WHERE p.trip_id = mt.id AND p.account_id = ? AND p.left_at IS NULL)))))";

/// The same for a mail with no arrival in the query: `MAIL_REACH`.
const MAIL_REACH: &str = "(m.account_id = ? OR (m.trip_id IS NOT NULL AND EXISTS (
     SELECT 1 FROM trips mt WHERE mt.id = m.trip_id AND (mt.account_id = ? OR EXISTS (
         SELECT 1 FROM trip_people p WHERE p.trip_id = mt.id AND p.account_id = ? AND p.left_at IS NULL)))))";
```

In `ARRIVAL_SELECT`, change `LEFT JOIN trips t ON t.id = a.trip_id AND t.account_id = a.account_id` to `LEFT JOIN trips t ON t.id = a.trip_id` (the slice-1 bug: a booking placed on a shared trip showed no trip name). The trip id comes only from placement or from the mail's own address, both of which name a trip the arrival's account reached.

- [ ] **Step 4: Through every query**

- `pending_arrivals_of_mail`: `WHERE a.mail_id = ? AND {ARRIVAL_REACH} AND a.status = 'pending' AND a.booking`, params `[mail_id, account_id, account_id, account_id]`.
- `arrival_of`: `WHERE a.id = ? AND {ARRIVAL_REACH}`, params `[id, account_id ×3]`.
- `decide_arrival`: the update cannot join, so:

```rust
    pub fn decide_arrival(&self, id: i64, account_id: i64, status: &str, item_id: Option<i64>) -> Result<bool> {
        let conn = self.conn();
        let changed = conn.execute(
            &format!(
                "UPDATE arrivals SET status = ?, item_id = ?, decided_at = now(), settled_by = ?
                 WHERE id = ? AND status = 'pending' AND booking
                   AND id IN (SELECT a.id FROM arrivals a JOIN inbound_mail m ON m.id = a.mail_id
                              WHERE a.id = ? AND {ARRIVAL_REACH})"
            ),
            params![status, item_id, account_id, id, id, account_id, account_id, account_id],
        )?;
        Ok(changed > 0)
    }
```

Update its doc: "…`false` when the arrival is not this account's to decide — its own, or its trip's — …". `reopen_arrival` also clears `settled_by = NULL`.

- `inbox_view`, pending: `WHERE {ARRIVAL_REACH} AND {UNDECIDED}`, params `[account_id ×3]`. Other mail: `WHERE {MAIL_REACH} AND m.received_at >= ?`, params `[account_id ×3, days_ago(30)]`.
- `delete_mail`: `WHERE m.id = ? AND {MAIL_REACH}`, params `[mail_id, account_id ×3]`; and where it deletes the mail's rows, also `DELETE FROM mail_forwards WHERE mail_id = ?`.
- `sweep_inbox`: add `conn.execute(&format!("DELETE FROM mail_forwards WHERE mail_id IN ({sweepable})"), params![cutoff])?;` before the arrivals delete.

New methods:

```rust
    /// Who settled an arrival, and how: `(status, account)`. `None` while
    /// it waits, or for one decided before step 22 recorded the presser.
    pub fn arrival_settler(&self, id: i64) -> Result<Option<(String, i64)>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT status, settled_by FROM arrivals WHERE id = ? AND status <> 'pending' AND settled_by IS NOT NULL",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// The people a trip's mail has been forwarded to.
    pub fn forwarded_to(&self, mail_id: i64) -> Result<Vec<i64>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT account_id FROM mail_forwards WHERE mail_id = ? ORDER BY account_id")?;
        let rows = stmt.query_map(params![mail_id], |r| r.get(0))?;
        Ok(rows.collect::<duckdb::Result<_>>()?)
    }

    /// One more person has their copy. Idempotent, so a retry that races a
    /// slow first pass does not fail on the key.
    pub fn note_forwarded_to(&self, mail_id: i64, account_id: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO mail_forwards (mail_id, account_id) VALUES (?, ?) ON CONFLICT DO NOTHING",
            params![mail_id, account_id],
        )?;
        Ok(())
    }
```

- [ ] **Step 5: Mail for a trip**

Rename `insert_mail`'s body to `fn insert_mail_within(&self, account_id: i64, trip_id: Option<i64>, provider_id: &str, …) -> Result<Option<i64>>`, adding `trip_id` to its `INSERT` column list and params. `insert_mail(…)` keeps its signature and calls it with `None`. Add:

```rust
    /// `insert_mail` for a mail that came to a trip's address: stored under
    /// the trip's owner, with the trip on it.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_trip_mail(
        &self,
        trip_id: i64,
        owner: i64,
        provider_id: &str,
        from: &str,
        subject: Option<&str>,
        text: Option<&str>,
        html: Option<&str>,
        truncated: bool,
        parts: &[MailPart],
    ) -> Result<Option<i64>> {
        self.insert_mail_within(owner, Some(trip_id), provider_id, from, subject, text, html, truncated, parts)
    }
```

`MailToWork` gains `pub trip_id: Option<i64>,`; `mail_to_work` selects `trip_id` last and fills it.

- [ ] **Step 6:** Run `cargo test --workspace` → PASS (existing inbox tests exercise the account path unchanged). Commit `feat(inbox): a trip's mail and its bookings reach everyone on the trip`.

---

### Task 4: Core — filing a trip's mail, and who settled it

**Files:** `crates/scout-core/src/inbox.rs`, `crates/scout-core/src/trips.rs`, `crates/scout-core/src/sharing.rs`

- [ ] **Step 1: Write the failing tests** (in `inbox.rs` tests; follow the module's existing `core()` helper)

```rust
    #[tokio::test]
    async fn a_trips_mail_is_filed_on_that_trip_without_asking_placement() {
        let (core, _dir, owner) = core_with_account().await;
        crate::trips::seed_trip_for_tests(&core, owner, "Lisbon").await.unwrap();
        crate::trips::keep(&core, owner, "Lisbon").await.unwrap();
        let address = crate::trips::find(&core, owner, "Lisbon").await.unwrap().unwrap().address.unwrap();
        let (trip_id, holder) = trip_for_address(&core, &address).await.unwrap().unwrap();
        assert_eq!(holder, owner);
        let mail = record_trip_mail(&core, trip_id, owner, mail_in("p-77")).await.unwrap().unwrap();
        // A booking in Porto in December would never be placed on Lisbon:
        // it came to Lisbon's address, so it goes there.
        let reading = Extraction { booking: true, title: Some("Hotel".into()), place: Some("Porto".into()),
            date: Some("2026-12-01".into()), ..Extraction::default() };
        let (_ids, placed) = record_arrivals(&core, owner, mail, vec![reading], Some(trip_id)).await.unwrap();
        assert_eq!(placed, Some(Placement::Trip(trip_id)));
    }

    #[tokio::test]
    async fn the_settler_is_named_for_whoever_asks() {
        let (core, _dir, owner) = core_with_account().await;
        crate::trips::seed_trip_for_tests(&core, owner, "Lisbon").await.unwrap();
        let masha = core.store().account_for_telegram(12).unwrap();
        crate::sharing::share_for_tests(&core, owner, "Lisbon", masha, "Masha").await.unwrap();
        let id = seed_arrival_on_trip_mail_for_tests(&core, owner, "Lisbon").await;
        ignore_arrival(&core, masha, id).await.unwrap();
        assert_eq!(settler(&core, owner, id).await.unwrap(), Some(Settled { added: false, by: "Masha".into() }));
        assert_eq!(settler(&core, masha, id).await.unwrap(), Some(Settled { added: false, by: "you".into() }));
    }
```

`core_with_account`, `mail_in` and `seed_arrival_on_trip_mail_for_tests` stand for the module's own helpers; where none fits, write them in the test module: `mail_in(id)` builds a `MailIn` with that provider id and empty body; `seed_arrival_on_trip_mail_for_tests` keeps the trip, records a trip mail and inserts one booking arrival with `trip_id` set, returning its id. `Extraction` needs `Default` (derive it if missing).

- [ ] **Step 2: Implement**

```rust
/// Whose trip `<raw>@domain` is: the trip and its owner. A local part not
/// shaped like a trip address cannot be one and never reaches the store.
pub async fn trip_for_address(core: &Core, raw: &str) -> anyhow::Result<Option<(i64, i64)>> {
    let Some(address) = normalise_trip_address(raw) else {
        return Ok(None);
    };
    let store = core.store();
    blocking(move || store.trip_for_address(&address)).await
}

/// `record_mail`, for a mail that came to a trip's address.
pub async fn record_trip_mail(core: &Core, trip_id: i64, owner: i64, mail: MailIn) -> anyhow::Result<Option<i64>> {
    let store = core.store();
    let id = blocking(move || {
        store.insert_trip_mail(
            trip_id,
            owner,
            &mail.provider_id,
            &mail.from,
            mail.subject.as_deref(),
            mail.text.as_deref(),
            mail.html.as_deref(),
            mail.truncated,
            &mail.parts,
        )
    })
    .await?;
    if id.is_some() {
        core.wake_inbox();
    }
    Ok(id)
}
```

`record_arrivals` gains a last parameter `trip: Option<i64>`. At the top of the blocking closure's placement block:

```rust
        let placement = if let Some(trip) = trip {
            // Sent to the trip's own address: there is nothing to work out.
            readings.iter().any(|e| e.booking).then_some(Placement::Trip(trip))
        } else {
            /* the existing block, unchanged */
        };
```

Every existing caller passes `None` (the worker changes in Task 6; `record_document` and tests).

```rust
/// Who decided an arrival, for a press that came second.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    pub added: bool,
    /// Their name on the arrival's trip, "you" when it was the asker.
    pub by: String,
}

pub async fn settler(core: &Core, account_id: i64, arrival_id: i64) -> anyhow::Result<Option<Settled>> {
    let store = core.store();
    blocking(move || {
        let Some(arrival) = store.arrival_of(arrival_id, account_id)? else { return Ok(None) };
        let Some((status, by)) = store.arrival_settler(arrival_id)? else { return Ok(None) };
        let by = if by == account_id {
            "you".to_string()
        } else {
            let names = match arrival.trip_id {
                Some(trip) => store.author_names(trip)?,
                None => Default::default(),
            };
            names.get(&by).cloned().unwrap_or_else(|| "someone on the trip".to_string())
        };
        Ok(Some(Settled { added: status == "added", by }))
    })
    .await
}
```

In `trips.rs`, `Plan` gains after `link`:

```rust
    /// The trip's own address, the local part; the page adds the domain.
    /// `None` on a draft.
    pub address: Option<String>,
```

`from_trip` sets `address: None`; `for_reader` sets it, and a trip whose address cannot be drawn is served without one rather than failing the whole list (the spec's "logged, served without one, tried again on the next serve"):

```rust
        plan.address = match store.trip_address(plan.trip.id) {
            Ok(address) => address,
            Err(e) => {
                tracing::warn!(error = %e, trip_id = plan.trip.id, "a trip is served without its address");
                None
            }
        };
``` Add `address: None` to the `Plan` literals in `nearby.rs` and `trip_pdf.rs`.

In `sharing.rs`:

```rust
/// A new address for the trip, for the owner.
pub async fn new_address(core: &Core, account_id: i64, trip: &str) -> anyhow::Result<Owned<String>> {
    let store = core.store();
    let trip = trip.to_string();
    blocking(move || {
        let Some(id) = trip_id(&store, account_id, &trip)? else { return Ok(Owned::NotFound) };
        Ok(match store.new_trip_address(account_id, id)? {
            Some(address) => Owned::Done(address),
            None => Owned::NotOwner,
        })
    })
    .await
}
```

- [ ] **Step 3:** Run and commit `feat(inbox): a trip's mail is filed on that trip, and a late press can be told who decided`.

---

### Task 5: Mail in at a trip's address

**Files:** `crates/scout-web/src/inbound.rs` (`receive` ~270)

- [ ] **Step 1: Write the failing test** (beside `the_webhook_stores_one_row_for_a_known_handle_and_nothing_otherwise`, reusing `inbound_app` and its signed-payload helper — the existing tests build a payload for a `to` address; use the same builder with the trip's address)

```rust
    #[tokio::test]
    async fn mail_to_a_trips_address_is_stored_for_that_trip() {
        let (app, core, _dir) = inbound_app(SECRET).await;
        let owner = /* the account `inbound_app`'s round admitted, as the handle tests get it */;
        scout_core::trips::seed_trip_for_tests(&core, owner, "Lisbon").await.unwrap();
        scout_core::trips::keep(&core, owner, "Lisbon").await.unwrap();
        let address = scout_core::trips::find(&core, owner, "Lisbon").await.unwrap().unwrap().address.unwrap();
        let res = deliver(&app, &format!("{address}@goodscout.fyi")).await;
        assert_eq!(res.status(), StatusCode::OK);
        let work = scout_core::inbox::mail_to_work(&core, 10).await.unwrap();
        assert_eq!(work.len(), 1);
        assert_eq!((work[0].account_id, work[0].trip_id.is_some()), (owner, true));
    }
```

`deliver` stands for the module's own signed-POST helper; give it the `to` address.

- [ ] **Step 2: Implement.** Replace the `Ok(None)` arm of the handle lookup:

```rust
        Ok(None) => match inbox::trip_for_address(&state.core, &handle).await {
            // A trip's own address: stored under its owner, with the trip.
            Ok(Some((trip_id, owner))) => return store(&state, owner, Some(trip_id), data).await,
            Ok(None) => {
                let shown: String = handle.chars().take(64).collect();
                tracing::info!(handle = shown, "mail for an address nobody holds");
                return StatusCode::OK.into_response();
            }
            Err(err) => {
                tracing::error!(%err, "looking up a trip address");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        },
```

and move the rest of the function (building `MailIn` and recording it) into `async fn store(state: &InboundState, account: i64, trip: Option<i64>, data: ReceivedData) -> Response`, which calls `inbox::record_trip_mail(&state.core, trip, account, mail)` when `trip` is `Some` and `inbox::record_mail` otherwise. The happy path for a handle becomes `store(&state, a, None, data).await`. (`ReceivedData` is whatever type `data` is in `receive`; use its real name.)

- [ ] **Step 3:** Run `cargo test -p scout-web inbound` → PASS. Commit `feat(inbound): mail to a trip's address is stored for that trip`.

---

### Task 6: The worker forwards and nudges once per person

**Files:** `crates/scout-web/src/inbox_worker.rs` (`process` ~200, `forward` ~363)

- [ ] **Step 1: Write the failing test** (in the worker's tests, which already stand Resend up with `wiremock` and count sends with `forwards(&requests)`; reuse their setup)

```rust
    #[tokio::test]
    async fn a_trips_mail_goes_to_everyone_on_it_once_and_never_back_to_its_sender() {
        // Owner sasha@ (email identity), editor masha@ (email identity and
        // a Telegram delivery), editor dan (Telegram only). The mail is
        // from masha@ — she booked it — so she gets no copy back.
        let (core, server, _dir) = /* the module's worker test setup */;
        let (owner, masha, dan) = /* three accounts with those identities */;
        scout_core::trips::seed_trip_for_tests(&core, owner, "Lisbon").await.unwrap();
        scout_core::sharing::share_for_tests(&core, owner, "Lisbon", masha, "Masha").await.unwrap();
        scout_core::sharing::share_for_tests(&core, owner, "Lisbon", dan, "Dan").await.unwrap();
        let trip = scout_core::trips::find(&core, owner, "Lisbon").await.unwrap().unwrap();
        let (trip_id, _) = scout_core::inbox::trip_for_address(&core, trip.address.as_deref().unwrap()).await.unwrap().unwrap();
        scout_core::inbox::record_trip_mail(&core, trip_id, owner, mail_from("masha@example.com")).await.unwrap();
        work_once(&core, &client(&server), FROM, 10).await;
        assert_eq!(forwards(&server.received_requests().await.unwrap()), 1, "to sasha only");
        // A second pass owes nothing.
        work_once(&core, &client(&server), FROM, 10).await;
        assert_eq!(forwards(&server.received_requests().await.unwrap()), 1);
        // Everyone with a phone was told, each under their own key.
        assert_eq!(queued_nudges(&core).await, vec![masha, dan]);
    }
```

The module's existing tests show how they make an account with an email identity (`identity::sign_in(core, "email", …)`), a Telegram delivery, a stubbed extraction and a `MailIn`; build `mail_from` and `queued_nudges` (reading `outbox` rows with `inbox:` keys) the same way.

- [ ] **Step 2: Forward per person.** At the top of `forward`, after the `m.forwarded` check:

```rust
    if let Some(trip_id) = m.trip_id {
        return forward_to_trip(core, client, from, m, trip_id, sender, subject, text, html).await;
    }
```

and:

```rust
/// A trip's mail, to each person on the trip with an address, one send
/// each so nobody sees another's address, never back to whoever sent it.
/// `mail_forwards` remembers who has theirs, so a retry after an outage
/// sends only what is still owed. `Yes` once nobody is owed one; `Later`
/// while somebody is and Resend was not there.
#[allow(clippy::too_many_arguments)]
async fn forward_to_trip(
    core: &Core,
    client: &ResendClient,
    from: &str,
    m: &MailToWork,
    trip_id: i64,
    sender: &str,
    subject: Option<String>,
    text: Option<String>,
    html: Option<String>,
) -> Result<Forwarded, Failure> {
    let people = scout_core::sharing::people_of(core, trip_id).await.map_err(Failure::Reading)?;
    let done = scout_core::inbox::forwarded_to(core, m.id).await.map_err(Failure::Reading)?;
    let all = scout_core::inbox::attachment_bytes(core, m.id).await.map_err(Failure::Reading)?;
    let attachments = within_cap(all, FORWARD_CAP);
    let mut later = false;
    for person in people.into_iter().filter(|p| !done.contains(p)) {
        let theirs = scout_core::inbox::emails_of(core, person).await.map_err(Failure::Reading)?;
        if scout_core::inbox::sender_is_the_account(sender, &theirs) {
            continue;
        }
        let Some(to) = theirs.first().cloned() else { continue };
        let mail = Outgoing {
            from: from.into(),
            to,
            reply_to: Some(sender.to_string()),
            subject: subject.clone().unwrap_or_default(),
            text: text.clone(),
            html: html.clone(),
            attachments: attachments.clone(),
        };
        match client.send(&mail).await {
            Ok(()) => scout_core::inbox::note_forwarded_to(core, m.id, person).await.map_err(Failure::Reading)?,
            Err(e) if crate::resend::is_transient(&e) => {
                tracing::warn!(error = %e, id = m.id, "a trip's forward did not go; sending later");
                later = true;
            }
            Err(e) => tracing::warn!(error = %e, id = m.id, "a trip's forward was refused"),
        }
    }
    if later {
        return Ok(Forwarded::Later);
    }
    scout_core::inbox::mail_forwarded(core, m.id).await.map_err(Failure::Reading)?;
    Ok(Forwarded::Yes)
}
```

(Use the real field names of `Outgoing` as `forward` builds it; `attachments` must be `Clone` — it is a `Vec<(String, Vec<u8>)>`.) Add the thin wrappers in core: `inbox::forwarded_to(core, mail_id)`, `inbox::note_forwarded_to(core, mail_id, account_id)`, and `sharing::people_of(core, trip_id) -> Vec<i64>` (the account ids of `trip_people`, owner first).

- [ ] **Step 3: File and nudge per person.** In `process`:

```rust
    let (_, placement) = scout_core::inbox::record_arrivals(core, m.account_id, m.id, readings, m.trip_id)
        .await
        .map_err(Failure::Reading)?;
```

and replace the final `nudge` call with:

```rust
    // A trip's mail is everyone's on the trip: each gets the line, under
    // their own key, and the buttons under it read their own reach.
    let told = match m.trip_id {
        Some(trip_id) => scout_core::sharing::people_of(core, trip_id).await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, id = m.id, "could not read who is on the trip; telling its owner");
            vec![m.account_id]
        }),
        None => vec![m.account_id],
    };
    for account in told {
        if let Err(e) = scout_core::inbox::nudge(core, account, m.id, &text).await {
            tracing::warn!(error = %e, id = m.id, "could not nudge about an arrival");
        }
    }
```

`nudge` already keys on `(account_id, channel, inbox:<mail_id>)`, so a retry tells nobody twice; `buttons_for` in the Telegram mirror reads `pending_of_mail` for the row's own account, which Task 3 widened.

- [ ] **Step 4:** Run and commit `feat(inbox): a trip's mail is forwarded to and announced to everyone on it`.

---

### Task 7: A late press says who decided

**Files:** `crates/scout-telegram/src/bot.rs` (`handle_callback` ~1355), `crates/scout-telegram/src/arrivals.rs`

- [ ] **Step 1: Write the failing test** (in `arrivals.rs` tests)

```rust
    #[test]
    fn a_press_that_came_second_says_who_came_first() {
        assert_eq!(already_line(true, "Masha"), "Already added by Masha.");
        assert_eq!(already_line(false, "Masha"), "Already ignored by Masha.");
        assert_eq!(already_line(true, "you"), "Already added by you.");
    }
```

- [ ] **Step 2: Implement**

```rust
/// What a press says when somebody else on the trip pressed first.
pub fn already_line(added: bool, by: &str) -> String {
    format!("Already {} by {by}.", if added { "added" } else { "ignored" })
}
```

In `handle_callback`, the `NotPending` arm:

```rust
        Ok(Outcome::NotPending) => match scout_core::inbox::settler(&app.core, account_id, arrival_id).await {
            Ok(Some(s)) => crate::arrivals::already_line(s.added, &s.by),
            _ => "Already decided — on the Trips tab, or from another press.".to_string(),
        },
```

The message edit that follows already appends the line and drops the decided buttons, which is the spec's "edits that one message".

- [ ] **Step 3:** Run and commit `feat(telegram): a late press on a trip's booking says who decided`.

---

### Task 8: Replacing the address

**Files:** `crates/scout-web/src/routes/trips.rs`

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn only_the_owner_replaces_a_trips_address_and_everyone_sees_it() {
        let (app, core, _dir, owner, cookie, csrf) = setup().await;
        let (masha, m_cookie, m_csrf) = second(&core).await;
        scout_core::sharing::share_for_tests(&core, owner, "October", masha, "Masha").await.unwrap();
        let listed: serde_json::Value =
            serde_json::from_str(&body_of(get_with_cookie(&app, "/chat/trips", &m_cookie).await).await).unwrap();
        let first = listed[0]["address"].as_str().unwrap().to_string();
        assert!(first.starts_with("october-"), "{first}");
        let res = post_json(&app, "/chat/trips/address-new", &m_cookie, Some(&m_csrf), r#"{"trip":"October"}"#).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let res = post_json(&app, "/chat/trips/address-new", &cookie, Some(&csrf), r#"{"trip":"October"}"#).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_ne!(body["address"].as_str().unwrap(), first);
    }
```

- [ ] **Step 2: Implement**: `.route("/chat/trips/address-new", post(new_address))` and a handler shaped like `link_off`:

```rust
    match scout_core::sharing::new_address(&auth.core, account_id, &body.trip).await {
        Ok(scout_core::sharing::Owned::Done(address)) => axum::Json(serde_json::json!({ "address": address })).into_response(),
        other => refused(other, account_id, "new address"),
    }
```

- [ ] **Step 3:** Run and commit `feat(web): the owner can replace a trip's address`.

---

### Task 9: The address on the page

**Files:** `crates/scout-web/src/chat.js`, `chat.test.mjs`, `chat.html`

- [ ] **Step 1: Write the failing tests**

```js
test('a trip with its own address is booked to that, else to the reader\'s', () => {
  const inbox = { handle: 'sasha', domain: 'goodscout.fyi' }
  assert.equal(bookingAddress({ address: 'lisbon-4f9qa' }, inbox), 'lisbon-4f9qa@goodscout.fyi')
  assert.equal(bookingAddress({ address: null }, inbox), 'sasha@goodscout.fyi')
  assert.equal(bookingAddress({}, { handle: null, domain: 'goodscout.fyi' }), null)
  assert.equal(bookingAddress({ address: 'lisbon-4f9qa' }, null), null, 'no domain known yet')
})
```

- [ ] **Step 2: Implement**

```js
// Where a confirmation for this trip should go: the trip's own address
// when it has one, so it lands here for everyone on it; the reader's own
// otherwise. Needs the domain, which the inbox carries.
export function bookingAddress(trip, inbox) {
  if (!inbox?.domain) return null
  if (trip?.address) return `${trip.address}@${inbox.domain}`
  return inbox.handle ? `${inbox.handle}@${inbox.domain}` : null
}
```

In `renderTripDetail`, `readinessAlert(trip, inboxAddress())` becomes `readinessAlert(trip, bookingAddress(trip, inbox))`.

In `peoplePanel`, after the people list and before the actions:

```js
    const address = trip.address && inbox?.domain ? `${trip.address}@${inbox.domain}` : null
    if (address) {
      const line = node('p', 'people-address')
      line.append(document.createTextNode('Trip email: '), node('code', '', address))
      const copyAddressButton = node('button', 'alert-copy', 'Copy')
      copyAddressButton.type = 'button'
      copyAddressButton.setAttribute('aria-label', `Copy ${address}`)
      copyAddressButton.addEventListener('click', () => copyAddress(address))
      line.append(copyAddressButton)
      panel.append(line)
    }
```

(appended to `panel` before `actions` — reorder the `panel.append(list, actions)` call to `panel.append(list)`, then the address, then `panel.append(actions)`). For the owner, add a third link action, `['New address', '/chat/trips/address-new', 'New address made. The old one no longer works.']`, to the loop that draws New link and Turn link off.

CSS in `chat.html` beside `.people-link-state`:

```css
  .people-address{margin:0 0 12px; color:var(--base1); font-size:13.5px; overflow-wrap:anywhere}
  .people-address code{font-size:13px}
```

- [ ] **Step 3:** Run node and Rust tests (the theme tests check the new CSS). Commit `feat(web): a trip's address in its People panel and its to-book banner`.

---

### Task 10: Verify, ship, record

- [ ] **Step 1:** The full gate; every result ok, clippy clean.
- [ ] **Step 2: Browser** — extend `scratchpad/share/server.mjs`: trips carry `address: 'lisbon-4f9qa'`, `/chat/inbox` answers `{handle:'sasha', domain:'goodscout.fyi', pending:[], other:[]}` (check the real path the page fetches the inbox from). At 390px and 1280px: the panel shows "Trip email: lisbon-4f9qa@goodscout.fyi" with Copy; the owner sees New address; the to-book banner names the trip's address. One screenshot per width.
- [ ] **Step 3:** Merge `--no-ff`, push, `scripts/deploy-k3s.sh`, and read the pod log for `backed up before migrating … to=22`, `applied migration step step=22`, `scout is up`.
- [ ] **Step 4: Live check.** `GET /chat/trips` for the user's account (only with their say-so) shows an `address` on kept trips. Sending a real mail to a trip address is the user's to do: it lands on their live trip.
- [ ] **Step 5:** `docs/BOARD.md` Done line and a board-artifact card; commit and push.
