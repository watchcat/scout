# In-Trip Mode, Slice 1: Today — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Opened on a date inside a trip, the trip page marks today, opens on today's cards, and a message sent from the Trips tab tells Scout what day it is.

**Architecture:** Everything about "today" is decided in the client from the browser's local date: `tripDayRows` learns to give today a row of its own, cards on that row are marked, and the page scrolls to them once per trip. The only server change is two optional fields on a chat message, turned into a `[system note]` on the prompt; the transcript already cuts such notes from what it shows.

**Tech Stack:** Plain JS (`crates/scout-web/src/chat.js`, tested with `node --test`), Rust/axum (`crates/scout-web`), `scout-core` session transcript.

**Spec:** `docs/superpowers/specs/2026-10-02-in-trip-mode-design.md`, "Slice 1 — Today".

**Repo rules that apply to every task**
- Never run `cargo fmt`.
- The gate, run before any merge:
  - `cargo test --workspace`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `node --test 'crates/scout-web/src/*.test.mjs'`
- Comments say why, not what. Match the surrounding comment density.
- Every commit message ends with a blank line and
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Work on branch `feat/in-trip-today`: `git checkout -b feat/in-trip-today` before Task 1.

## File map

| File | Change |
|---|---|
| `crates/scout-web/src/chat.js` | `tripDayRows` gains `today`; new exports `localToday`, `todayInTrip`, `todayPositions`, `composeLabel`; `sendBody` gains `about`; drawing, marking, scrolling, compose target |
| `crates/scout-web/src/chat.test.mjs` | tests for the above |
| `crates/scout-web/src/chat.html` | CSS for today's row and today's cards |
| `crates/scout-web/src/routes/chat.rs` | `MessageIn.trip`, `MessageIn.today`, `today_note`, used in `send_message` |
| `crates/scout-core/src/session.rs` | one test pinning that the note is not shown |
| `README.md`, `docs/BOARD.md` | one paragraph, one Done line |

---

### Task 1: Today as a row of its own

**Files:**
- Modify: `crates/scout-web/src/chat.js` (the `tripDayRows` export, about line 530)
- Test: `crates/scout-web/src/chat.test.mjs`

- [ ] **Step 1: Write the failing tests**

Add `localToday, todayInTrip, todayPositions` to the names imported from `./chat.js` at the top of `chat.test.mjs` (`tripDayRows` is already imported). Append:

```js
// Today is a position in the trip, and it gets a row even where the trip
// has nothing on it: "where am I in this" is the question on a trip day.
const todayTrip = {
  items: [
    { position: 1, kind: 'activity', title: 'eSIM', date: '2026-09-12', booked: true },
    { position: 2, kind: 'activity', title: 'Lunch', date: '2026-09-24', starts_at: '2026-09-24T14:30:00', booked: false },
    { position: 3, kind: 'activity', title: 'Tour', date: '2026-09-26', booked: false },
  ],
}
const todayShape = (rows) => rows.map((row) => row.kind === 'free'
  ? `free ${row.days}`
  : `${row.date}${row.today ? '*' : ''}:${row.entries.length}`)

test('today inside a run of free days splits the run around a row of its own', () => {
  assert.deepEqual(todayShape(tripDayRows(todayTrip, 'en-GB', '2026-09-18')), [
    '2026-09-12:1', 'free 5', '2026-09-18*:0', 'free 5', '2026-09-24:1', '2026-09-25:0', '2026-09-26:1',
  ])
  // First day of a run: nothing before it to count.
  assert.deepEqual(todayShape(tripDayRows(todayTrip, 'en-GB', '2026-09-13')), [
    '2026-09-12:1', '2026-09-13*:0', 'free 10', '2026-09-24:1', '2026-09-25:0', '2026-09-26:1',
  ])
})

test('today on a day with something on it, or on the single empty day, marks that row and adds none', () => {
  assert.deepEqual(todayShape(tripDayRows(todayTrip, 'en-GB', '2026-09-24')), [
    '2026-09-12:1', 'free 11', '2026-09-24*:1', '2026-09-25:0', '2026-09-26:1',
  ])
  assert.deepEqual(todayShape(tripDayRows(todayTrip, 'en-GB', '2026-09-25')), [
    '2026-09-12:1', 'free 11', '2026-09-24:1', '2026-09-25*:0', '2026-09-26:1',
  ])
  assert.deepEqual(todayPositions(tripDayRows(todayTrip, 'en-GB', '2026-09-24')), [2])
  assert.deepEqual(todayPositions(tripDayRows(todayTrip, 'en-GB', '2026-09-25')), [])
})

test('a today outside the trip changes nothing, and the day number counts both ends', () => {
  assert.deepEqual(tripDayRows(todayTrip, 'en-GB', '2026-10-01'), tripDayRows(todayTrip, 'en-GB'))
  assert.deepEqual(tripDayRows(todayTrip, 'en-GB', null), tripDayRows(todayTrip, 'en-GB'))
  assert.deepEqual(todayInTrip(todayTrip, '2026-09-12'), { day: 1, of: 15 })
  assert.deepEqual(todayInTrip(todayTrip, '2026-09-26'), { day: 15, of: 15 })
  assert.equal(todayInTrip(todayTrip, '2026-09-11'), null)
  assert.equal(todayInTrip(todayTrip, '2026-09-27'), null)
  assert.equal(todayInTrip(todayTrip, 'yesterday'), null)
  assert.equal(todayInTrip({ items: [] }, '2026-09-12'), null)
})

test('today is the reader\'s own calendar day, not the UTC one', () => {
  // 23:30 local on the 3rd is the 3rd, whatever UTC says.
  assert.equal(localToday(new Date(2026, 8, 3, 23, 30)), '2026-09-03')
  assert.equal(localToday(new Date(2026, 0, 9, 0, 5)), '2026-01-09')
})
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `node --test crates/scout-web/src/chat.test.mjs 2>&1 | grep -E "^# (pass|fail)|SyntaxError"`
Expected: a SyntaxError naming `localToday` as a missing export, `# fail 1`.

- [ ] **Step 3: Implement**

In `chat.js`, change the signature and the loop of `tripDayRows`. Replace

```js
export function tripDayRows(trip, locale = undefined) {
```

with

```js
// `today` is the reader's own date, when the page wants it marked. A day
// that is today is a row even with nothing on it, and so splits a counted
// run of free days in two: on a trip day "where am I in this" is the
// question, and a count that swallowed today could not answer it. Left
// out, the rows are exactly what the printed plan draws.
export function tripDayRows(trip, locale = undefined, today = null) {
```

and replace the body of the `for (const date of eachDay(...))` loop

```js
    const entries = (onDay.get(date) ?? []).sort(byTime)
    if (!entries.length) {
      free++
      continue
    }
    // A single empty day is worth a row; two or more are worth a count.
    if (free === 1) rows.push({ kind: 'day', date: dayBefore(date), label: dayLabel(dayBefore(date), locale), entries: [] })
    else if (free > 1) rows.push({ kind: 'free', days: free })
    free = 0
    rows.push({ kind: 'day', date, label: dayLabel(date, locale), entries })
```

with

```js
    const entries = (onDay.get(date) ?? []).sort(byTime)
    const isToday = date === today
    if (!entries.length && !isToday) {
      free++
      continue
    }
    // A single empty day is worth a row; two or more are worth a count.
    if (free === 1) rows.push({ kind: 'day', date: dayBefore(date), label: dayLabel(dayBefore(date), locale), entries: [] })
    else if (free > 1) rows.push({ kind: 'free', days: free })
    free = 0
    const row = { kind: 'day', date, label: dayLabel(date, locale), entries }
    // Only where true, so a row without it is the row the shared
    // `day_rows.json` cases describe.
    if (isToday) row.today = true
    rows.push(row)
```

Directly after the closing brace of `tripDayRows`, add:

```js
// The reader's own calendar day, written the way the trip's days are.
// Local on purpose: the phone is where the traveller is, and the UTC date
// is yesterday's for the first hours of a morning in Hong Kong.
export function localToday(now = new Date()) {
  const two = (n) => String(n).padStart(2, '0')
  return `${now.getFullYear()}-${two(now.getMonth() + 1)}-${two(now.getDate())}`
}

// Which day of the trip `today` is — both ends counted — or `null` when it
// is not a day of this trip at all, which is the case that changes nothing.
export function todayInTrip(trip, today) {
  if (!isDay(today)) return null
  const days = tripDayRows(trip).filter((row) => row.kind === 'day')
  if (!days.length) return null
  const first = days[0].date
  const last = days[days.length - 1].date
  if (today < first || today > last) return null
  const between = (from, to) => Math.round((utcDay(to) - utcDay(from)) / DAY_MS)
  return { day: between(first, today) + 1, of: between(first, last) + 1 }
}

// The items on today's row, by position: the cards the page marks and
// opens on. Read off the rows so the cards and the overview cannot
// disagree about what today holds.
export function todayPositions(rows) {
  const row = rows.find((candidate) => candidate.kind === 'day' && candidate.today)
  return row ? [...new Set(row.entries.map((entry) => entry.position))] : []
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `node --test 'crates/scout-web/src/*.test.mjs' 2>&1 | grep -E "^# (pass|fail)"`
Expected: `# fail 0`. The shared `day_rows.json` cases still pass, which is what proves the printed plan's rows are untouched.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-web/src/chat.js crates/scout-web/src/chat.test.mjs
git commit -m "feat(trips): today gets a row of its own in the day rows

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: Mark today, and open on it

**Files:**
- Modify: `crates/scout-web/src/chat.js` (inside `start()`: `renderOverview`, `nameCard`, `renderTripDetail`, `switchView`)
- Modify: `crates/scout-web/src/chat.html` (CSS)
- Test: `crates/scout-web/src/chat.test.mjs`

- [ ] **Step 1: Write the failing test**

Append to `chat.test.mjs`:

```js
// The marks are CSS the script only names: lose either rule and today
// looks like every other day while every test of the rows still passes.
test('the page has a look for today\'s row and today\'s cards', () => {
  const page = readFileSync(new URL('./chat.html', import.meta.url), 'utf8')
  assert.match(page, /\.day-row\.today \.day-label\{/)
  assert.match(page, /\.day-today\{/)
  assert.match(page, /\[data-today\]\{/)
  const script = readFileSync(new URL('./chat.js', import.meta.url), 'utf8')
  assert.match(script, /card\.dataset\.today = ''/)
  assert.match(script, /'day-row today'/)
})
```

- [ ] **Step 2: Run it to verify it fails**

Run: `node --test crates/scout-web/src/chat.test.mjs 2>&1 | grep -E "^# (pass|fail)"`
Expected: `# fail 1`.

- [ ] **Step 3: Add the CSS**

In `chat.html`, directly after the line that starts `  .day-chip:focus-visible{outline:2px solid var(--blue); outline-offset:2px}`, add:

```css
  /* Today is a position in the trip, not a state of anything on it: the
     chips keep their held and to-book looks, and the day says it is today.
     The row itself is `display:contents`, so the mark is on its label. */
  .day-row.today .day-label{color:var(--base2)}
  .day-today{display:block; color:var(--cyan); font-size:10px; font-weight:800; letter-spacing:.1em;
    text-transform:uppercase}
  /* An inset shadow, not a border: the card clips its corners, and a
     border would move everything in it by three pixels on the one day. */
  .segment-card[data-today], .item-card[data-today]{box-shadow:inset 3px 0 0 var(--cyan)}
```

- [ ] **Step 4: Draw it**

In `chat.js`, inside `start()`, next to `let currentTrip = null`, add:

```js
  // The positions on today's row of the trip being drawn, and the trip the
  // page has already opened on today for. See `openOnToday`.
  let todayCards = new Set()
  let openedOnToday = null
```

Add these two functions directly above `function renderOverview`:

```js
  // Today's date when it is a day of this trip, else `null` — the one
  // answer every part of the page asks for.
  function todayOf(trip) {
    const today = localToday()
    return todayInTrip(trip, today) ? today : null
  }

  // Opens the trip on today, once. A repaint after a note or a move must
  // leave the reader where they were, so this remembers the trip it did it
  // for; choosing another trip is a different name and does it again.
  // Skipped while the Trips tab is hidden — the website paints trips in
  // the background for the tab's count, and scrolling something nobody can
  // see would spend the one go.
  function openOnToday(trip) {
    if (tripsView.hidden || !trip || openedOnToday === trip.name) return
    openedOnToday = trip.name
    const target = tripDetail.querySelector('[data-today]')
      ?? tripDetail.querySelector('.day-row.today .day-label')
    target?.scrollIntoView({ block: 'start' })
  }
```

In `renderOverview`, replace

```js
    for (const row of tripDayRows(trip)) {
      if (row.kind === 'free') {
        days.append(node('p', 'day-free', `${row.days} free days`))
        continue
      }
      const line = node('div', 'day-row')
      line.append(node('span', 'day-label', row.label))
```

with

```js
    for (const row of tripDayRows(trip, undefined, todayOf(trip))) {
      if (row.kind === 'free') {
        days.append(node('p', 'day-free', `${row.days} free days`))
        continue
      }
      const line = node('div', row.today ? 'day-row today' : 'day-row')
      const label = node('span', 'day-label', row.label)
      if (row.today) label.append(node('span', 'day-today', 'Today'))
      line.append(label)
```

In `nameCard`, replace

```js
    card.id = `trip-item-${item.position}`
    card.tabIndex = -1
```

with

```js
    card.id = `trip-item-${item.position}`
    card.tabIndex = -1
    if (todayCards.has(item.position)) card.dataset.today = ''
```

In `renderTripDetail`, replace the line `    tripDetail.replaceChildren()` with

```js
    tripDetail.replaceChildren()
    todayCards = new Set(todayPositions(tripDayRows(trip, undefined, todayOf(trip))))
    // After this function has drawn the cards, which is the rest of it:
    // a microtask runs when the synchronous render is done.
    queueMicrotask(() => openOnToday(trip))
```

In `switchView`, directly after the line `    if (showingTrips && !tripsLoaded) loadTrips().catch(() => {})`, add:

```js
    // The trips were painted while this tab was hidden; now it can be seen.
    if (showingTrips) openOnToday(trips.find((trip) => trip.name === currentTrip))
```

- [ ] **Step 5: Run the tests**

Run: `node --test 'crates/scout-web/src/*.test.mjs' 2>&1 | grep -E "^# (pass|fail)"`
Expected: `# fail 0`.

- [ ] **Step 6: Commit**

```bash
git add crates/scout-web/src/chat.js crates/scout-web/src/chat.html crates/scout-web/src/chat.test.mjs
git commit -m "feat(trips): the page marks today and opens on today's cards

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: The composer says what day it is

**Files:**
- Modify: `crates/scout-web/src/chat.js` (`sendBody`, `runMessage`, `updateComposeTarget`)
- Test: `crates/scout-web/src/chat.test.mjs`

- [ ] **Step 1: Write the failing tests**

Add `composeLabel` to the names imported from `./chat.js` (`sendBody` is already imported). Append:

```js
test('a message carries the trip and today only when it has both', () => {
  assert.deepEqual(JSON.parse(sendBody('hi', 7)), { text: 'hi', thread: 7 })
  assert.deepEqual(JSON.parse(sendBody('hi', 7, null)), { text: 'hi', thread: 7 })
  assert.deepEqual(JSON.parse(sendBody('hi', 7, { trip: 'Hong Kong', today: '2026-09-23', day: 3, of: 9 })), {
    text: 'hi', thread: 7, trip: 'Hong Kong', today: '2026-09-23',
  })
  assert.deepEqual(JSON.parse(sendBody('hi', 7, { trip: 'Hong Kong' })), { text: 'hi', thread: 7 })
})

test('the line above the composer says which day of the trip it is', () => {
  assert.equal(composeLabel('to "Hong Kong"', { day: 3, of: 9 }), 'to "Hong Kong" · today is day 3 of 9')
  assert.equal(composeLabel('to "Hong Kong"', null), 'to "Hong Kong"')
  // Nothing to name is still nothing to say.
  assert.equal(composeLabel('', { day: 3, of: 9 }), '')
})
```

- [ ] **Step 2: Run them to verify they fail**

Run: `node --test crates/scout-web/src/chat.test.mjs 2>&1 | grep -E "^# (pass|fail)|SyntaxError"`
Expected: a SyntaxError naming `composeLabel`, `# fail 1`.

- [ ] **Step 3: Implement**

In `chat.js`, replace

```js
export function sendBody(text, thread) {
  return JSON.stringify({ text, thread })
}
```

with

```js
// `about` is the trip the message was sent from and the reader's own
// date, on a day of that trip — so "move the ferry to tomorrow" has a
// tomorrow. Both or neither: half of it tells the server nothing.
export function sendBody(text, thread, about = null) {
  const body = { text, thread }
  if (about?.trip && about?.today) {
    body.trip = about.trip
    body.today = about.today
  }
  return JSON.stringify(body)
}

// What the line above the composer says, with the day of the trip when
// today is one.
export function composeLabel(label, at) {
  return label && at ? `${label} · today is day ${at.day} of ${at.of}` : label
}
```

Inside `start()`, directly above `function updateComposeTarget`, add:

```js
  // The trip on screen and today's date, when today is a day of it.
  function tripToday() {
    const trip = trips.find((item) => item.name === currentTrip)
    const today = localToday()
    const at = trip ? todayInTrip(trip, today) : null
    return at ? { trip: trip.name, today, ...at } : null
  }
```

In `updateComposeTarget`, replace

```js
    composeTargetEl.hidden = !target.label
    composeTargetEl.textContent = target.label ? `↩ ${target.label}` : ''
```

with

```js
    const label = composeLabel(target.label, tripToday())
    composeTargetEl.hidden = !label
    composeTargetEl.textContent = label ? `↩ ${label}` : ''
```

In `runMessage`, directly after the line `    const runThread = currentThread`, add:

```js
    // Decided here, where the send begins: `currentTrip` is still the trip
    // the reader was looking at, though the view has gone to Chat.
    const about = fromTrips ? tripToday() : null
```

and change `        body: sendBody(text, runThread),` to `        body: sendBody(text, runThread, about),`.

- [ ] **Step 4: Run the tests**

Run: `node --test 'crates/scout-web/src/*.test.mjs' 2>&1 | grep -E "^# (pass|fail)"`
Expected: `# fail 0`.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-web/src/chat.js crates/scout-web/src/chat.test.mjs
git commit -m "feat(trips): a message from a trip day says which trip and what day

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: The server turns it into a note the model sees and the thread does not show

**Files:**
- Modify: `crates/scout-web/src/routes/chat.rs` (`MessageIn`, `send_message`, new `today_note`)
- Test: `crates/scout-web/src/routes/chat.rs` (its `mod tests`), `crates/scout-core/src/session.rs` (its `mod tests`)

- [ ] **Step 1: Pin that the transcript hides the note**

This behaviour already exists; the test pins it for this note's wording. In the `mod tests` of `crates/scout-core/src/session.rs`, directly after the test `an_instruction_scout_gave_itself_is_not_shown_as_the_persons_words`, add:

```rust
    #[test]
    fn the_note_a_trip_day_adds_is_not_shown_as_the_persons_words() {
        // The page sends the trip and the day; the server appends them as
        // a note the model reads. What the thread shows is what was typed.
        let history = vec![
            LlmMessage::user(
                "move the ferry to tomorrow\n\n[system note] Sent from the trip \"Hong Kong\". Today is Wed 23 Sep 2026, a day of that trip.",
            ),
            LlmMessage::assistant("Moved."),
        ];
        let turns = turns_of(&history, &vec![None; history.len()]);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].text, "move the ferry to tomorrow");
    }
```

Run: `cargo test -p scout-core the_note_a_trip_day_adds 2>&1 | grep "test result"`
Expected: `1 passed`. If it fails, stop: the note needs a cut in `turns_of` before anything else in this task is built, and the fix is to make the cut that handles `[system note] This is a cheapest-price request.` handle this one.

- [ ] **Step 2: Write the failing route-side test**

In the `mod tests` of `crates/scout-web/src/routes/chat.rs`, add:

```rust
    #[tokio::test]
    async fn a_message_from_a_trip_day_carries_the_day_and_nothing_the_client_made_up() {
        let (_app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        scout_core::trips::seed_trip_for_tests(&core, account_id, "October").await.unwrap();

        // The trip's own name as stored, not the client's spelling of it,
        // and the weekday worked out here.
        let note = today_note(&core, account_id, Some("october"), Some("2026-10-12")).await;
        assert_eq!(
            note.as_deref(),
            Some("\n\n[system note] Sent from the trip \"October\". Today is Mon 12 Oct 2026, a day of that trip.")
        );
        // Dropped whole when either half is missing or not what it says.
        assert_eq!(today_note(&core, account_id, Some("October"), Some("12 Oct")).await, None);
        assert_eq!(today_note(&core, account_id, Some("Nowhere"), Some("2026-10-12")).await, None);
        assert_eq!(today_note(&core, account_id, None, Some("2026-10-12")).await, None);
        assert_eq!(today_note(&core, account_id, Some("October"), None).await, None);
        // Somebody else's trip is no trip.
        let other = admitted(&core, "888").await;
        assert_eq!(today_note(&core, other, Some("October"), Some("2026-10-12")).await, None);
    }
```

Run: `cargo test -p scout-web a_message_from_a_trip_day 2>&1 | grep -E "^error|test result" | head -3`
Expected: a compile error, `cannot find function today_note`.

- [ ] **Step 3: Implement**

In `routes/chat.rs`, in `struct MessageIn`, after the field `thread: i64,` add:

```rust
    /// The trip this was sent from and the reader's own date, when that
    /// date is a day of the trip. Optional and defaulted: an older page,
    /// and every message typed in Chat, sends neither.
    #[serde(default)]
    trip: Option<String>,
    #[serde(default)]
    today: Option<String>,
```

Directly above `async fn send_message`, add:

```rust
/// The note a message from a trip's page carries on a day of that trip,
/// or `None`.
///
/// Both halves are checked before either reaches the model: the date has
/// to parse and the name has to be one of this account's trips, and it is
/// the stored name that is written, not the client's string — a field of
/// the request is not a place to put words into the prompt.
///
/// The model reads it; the thread does not show it. `turns_of` cuts a
/// trailing `[system note]` from a turn, which is what Telegram's
/// price-request note already relies on.
async fn today_note(
    core: &scout_core::core::Core,
    account_id: i64,
    trip: Option<&str>,
    today: Option<&str>,
) -> Option<String> {
    let (trip, today) = (trip?, today?);
    let today = chrono::NaiveDate::parse_from_str(today, "%Y-%m-%d").ok()?;
    let plan = scout_core::trips::find(core, account_id, trip).await.ok()??;
    Some(format!(
        "\n\n[system note] Sent from the trip \"{}\". Today is {}, a day of that trip.",
        plan.trip.name,
        today.format("%a %-d %b %Y")
    ))
}
```

In `send_message`, directly above the line `    let run = scout_api::RunContext {`, add:

```rust
    let note = today_note(&auth.core, account_id, body.trip.as_deref(), body.today.as_deref()).await;
```

and replace the line `    let text = body.text;` with

```rust
    // The title above was cut from the person's own words; this is what
    // the model is sent.
    let text = match note {
        Some(note) => format!("{}{note}", body.text),
        None => body.text,
    };
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p scout-web a_message_from_a_trip_day 2>&1 | grep "test result"`
Expected: `1 passed`.

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -E "^error" -A6 | head -20`
Expected: no output.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-web/src/routes/chat.rs crates/scout-core/src/session.rs
git commit -m "feat(chat): a trip-day message reaches the model with the trip and the day

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: See it in a browser

Nothing in the repo changes in this task unless it finds a bug. It runs the real `chat.html` and `chat.js` against a stand-in server whose trip is dated around today.

- [ ] **Step 1: Write the stand-in and the check**

In a scratch directory outside the repo (`mkdir -p "$SCRATCH/today" && cd "$SCRATCH/today"`, then `ln -sfn "$(npm root -g)" node_modules` so `playwright` resolves), create `server.mjs`:

```js
import http from 'node:http'
import { readFileSync } from 'node:fs'
const SRC = '/Users/watchcat/work/rust/scout/crates/scout-web/src'
const day = (offset) => {
  const d = new Date(Date.now() + offset * 86400000)
  const two = (n) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${two(d.getMonth() + 1)}-${two(d.getDate())}`
}
const item = (position, title, date, extra = {}) => ({
  position, kind: 'activity', title, place: null, origin: null, destination: null, date,
  starts_at: null, ends_at: null, booked: false, confirmation_code: null, price: null, currency: null,
  notes: null, arrival_id: null, candidates: [], attachments: [], ...extra,
})
const trip = {
  name: 'Hong Kong', adults: 1, cabin_class: null, status: 'planning', kept: true,
  readiness: { state: 'no_flights' }, not_ready: null, to_book: [], notes: [], chat: null,
  items: [
    item(1, 'eSIM', day(-9), { booked: true }),
    item(2, 'Harbour walk', day(-1)),
    item(3, 'Moomin shop', day(0), { starts_at: `${day(0)}T11:00:00` }),
    item(4, 'Lunch with Stanley', day(0), { starts_at: `${day(0)}T14:30:00`, booked: true }),
    item(5, 'Star Ferry', day(2)),
    item(6, 'Peak tram', day(6)),
  ],
}
http.createServer((req, res) => {
  let body = ''
  req.on('data', (c) => { body += c })
  req.on('end', () => {
    const send = (code, type, data) => { res.writeHead(code, { 'content-type': type }); res.end(data) }
    const page = () => readFileSync(`${SRC}/chat.html`, 'utf8').replace('<!--CSRF-->', 't')
    if (req.url === '/') return send(200, 'text/html', page())
    if (req.url.startsWith('/chat?in=telegram')) return send(200, 'text/html', page().replace('<html lang="en">', '<html lang="en" data-surface="telegram">'))
    if (req.url === '/chat.js') return send(200, 'text/javascript', readFileSync(`${SRC}/chat.js`))
    if (req.url === '/telegram.js') return send(200, 'text/javascript', readFileSync(`${SRC}/telegram.js`))
    if (req.url === '/chat/trips') return send(200, 'application/json', JSON.stringify([trip]))
    if (req.url === '/chat/trips/item-note') {
      const b = JSON.parse(body)
      trip.items.find((i) => i.position === b.position).notes = b.note || null
      return send(200, 'application/json', JSON.stringify(trip))
    }
    send(404, 'text/plain', 'no')
  })
}).listen(8736, () => console.log('up'))
```

and `check.mjs`:

```js
import { chromium, devices } from 'playwright'
const browser = await chromium.launch()
for (const [name, opts, url] of [
  ['phone-miniapp', { ...devices['iPhone 13'], viewport: { width: 390, height: 844 } }, 'http://127.0.0.1:8736/chat?in=telegram'],
  ['desktop-site', { viewport: { width: 1360, height: 900 } }, 'http://127.0.0.1:8736/'],
]) {
  const page = await browser.newPage(opts)
  page.on('pageerror', (e) => console.log(name, 'pageerror', e.message))
  await page.goto(url, { waitUntil: 'networkidle' })
  if (name === 'desktop-site') await page.getByRole('button', { name: /^Trips/ }).click()
  await page.waitForSelector('.item-card')
  await page.waitForTimeout(200)
  const first = page.locator('[data-today]').first()
  const top = Math.round((await first.boundingBox()).y)
  console.log(name, 'today cards:', await page.locator('[data-today]').count(), 'first today card top:', top)
  console.log(name, 'today row label:', await page.locator('.day-row.today .day-label').innerText())
  // A repaint must not scroll: go to the top, add a note, look again.
  await page.locator('.trip-title').scrollIntoViewIfNeeded()
  const before = Math.round((await page.locator('.trip-title').boundingBox()).y)
  await page.getByRole('button', { name: 'Add note on Harbour walk' }).click()
  await page.getByRole('textbox', { name: 'Note on Harbour walk' }).fill('bring water')
  await page.getByRole('button', { name: 'Save' }).click()
  await page.waitForTimeout(300)
  const title = await page.locator('.trip-title').boundingBox()
  console.log(name, 'after a note the page stayed put:', title !== null && Math.abs(Math.round(title.y) - before) < 80)
  await page.screenshot({ path: `${name}.png` })
  await page.close()
}
await browser.close()
```

- [ ] **Step 2: Run it**

```bash
(node server.mjs > server.log 2>&1 &) ; sleep 0.5 ; node check.mjs 2>&1 | grep -v 404 ; pkill -f "node server.mjs"
```

Expected, for both `phone-miniapp` and `desktop-site`:
- `today cards: 2`
- `first today card top:` under 200 on the phone, under 300 on the desktop — the card is at the top of the scroller, not below the overview
- `today row label:` contains `Today`
- `after a note the page stayed put: true`
- no `pageerror` lines

Open `phone-miniapp.png` and `desktop-site.png` and look: today's two cards carry a cyan left edge, and the overview's row for today says "Today" under its date.

- [ ] **Step 3: If anything is off**

A `first today card top` far below the limit means `openOnToday` ran before the cards were drawn or while the tab was hidden; check the `queueMicrotask` in `renderTripDetail` and the call in `switchView` from Task 2. Fix, re-run Step 2, and commit the fix with a message that says what was wrong.

---

### Task 6: Gate, docs, ship

**Files:**
- Modify: `README.md`, `docs/BOARD.md`

- [ ] **Step 1: README**

In `README.md`, in the section `### Trips in the browser`, after the paragraph that ends `each marked when it is booked and with its confirmation code.`, add:

```markdown
On a day of the trip the page knows it. Today gets a row of its own in the
day rows, even inside a run of free days; the page opens on today's cards;
and a message typed from the Trips tab tells Scout which trip it came from
and what day it is, so "move the ferry to tomorrow" has a tomorrow.
```

- [ ] **Step 2: The gate**

```bash
cargo test --workspace 2>&1 | grep -E "FAILED|panicked|test result" | sort | uniq -c
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -E "^error" -A6 | head
node --test 'crates/scout-web/src/*.test.mjs' 2>&1 | grep -E "^# (pass|fail)"
```

Expected: every `test result` line says `ok`, no clippy output, `# fail 0`.

- [ ] **Step 3: Commit, merge, push**

```bash
git add README.md
git commit -m "docs: today in the trip

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
git checkout main
git merge --no-ff feat/in-trip-today -m "Merge: today in the trip

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
git push origin main
git branch -d feat/in-trip-today
```

- [ ] **Step 4: Deploy and verify**

```bash
scripts/deploy-k3s.sh
ssh $(sed -n 's/^SCOUT_SSH=//p' .env) 'kubectl -n scout get pods -l app=scout; kubectl -n scout logs -l app=scout --tail=40' | grep -E "Running|Crash|scout is up|Telegram delivers"
curl -s https://goodscout.fyi/chat.js | grep -c "openOnToday"
```

Expected: the pod `Running`, the lines `scout is up` and `Telegram delivers here`, and a count of at least 1.

- [ ] **Step 5: The board**

In `docs/BOARD.md`, directly under `## Done`, add one line, with `<hash>` replaced by the merge commit's short hash (`git log --merges --format=%h -1`):

```markdown
- [x] 2026-10-02 — **Today in the trip** (`<hash>`): on a day of the trip the page gives today a row of its own — splitting a counted run of free days around it — marks today's cards, and opens on them once per trip, so a repaint after a note leaves the reader where they were. A message typed from the Trips tab carries the trip and the reader's own date; the server checks both, writes the stored trip name and the weekday into a system note the model reads, and the thread shows only what was typed. Website only for the note: the Mini App has no composer. First slice of in-trip mode.
```

Commit and push:

```bash
git add docs/BOARD.md
git commit -m "docs(board): today in the trip

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
git push origin main
```

Then add the same card to the board artifact (`https://claude.ai/code/artifact/9be3e94c-0416-4f18-86a6-528a92847c64`, collection `cards`): a document `in-trip-today` with `area: "trips"`, `column: "done"`, the title "Today in the trip", the commit hash, and the BOARD.md sentence as `detail`.
