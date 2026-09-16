// The chat page's client. Pure helpers first (exported for
// `chat.test.mjs`), then the DOM wiring that uses them.

/// Moves accumulated text forward by one `TextUpdate`. The counterpart of
/// `TextUpdate::apply` in scout-api, and the reason it exists at all: a
/// `</think>` with no opener retracts everything already sent by replacing
/// it with `""`, and a client that only ever appended would leave that
/// stripped reasoning on screen forever. `Replace` must clear, not extend.
export function applyUpdate(answer, update) {
  if ('Append' in update) return answer + update.Append
  if ('Replace' in update) return update.Replace
  return answer
}

// What the bubble should show once the stream's `end` frame arrives.
//
// The bubble up to this point was built from token deltas, and those are
// every turn of a multi-turn run concatenated — including the "let me check
// the next shop" narration the model writes between tool calls, and any
// link the run's dead-link repair removed afterwards. The `end` frame
// carries what the run actually answered, so an `ok` end replaces the
// bubble outright. Replacing with an empty string is deliberate: it means
// the run produced nothing but reasoning, and the bubble has to clear.
// A `busy` or `error` end clears it for the same reason — neither produced
// an answer, and the notice beside it is the whole message.
//
// A missing `answer` is a different instruction from an empty one, and is
// not a retraction. A server part-way through a rollout still sends
// `{"status":"ok"}` on its own, and blanking a good answer because of a
// deploy would be worse than showing what streamed.
export function finalAnswer(end, streamed) {
  if (!end) return streamed
  if (end.status === 'ok') {
    return typeof end.answer === 'string' ? end.answer : streamed
  }
  // A run that failed, or never started, produced no answer. What streamed
  // is the model's working — the narration it writes between tool calls —
  // and leaving that above an apology reads as an answer cut off
  // mid-sentence rather than as nothing.
  if (end.status === 'error' || end.status === 'busy') return ''
  // An end this client does not recognise is not a reason to throw away
  // what the reader can already see.
  return streamed
}

// `&` first, or escaping `<`/`>` into `&lt;`/`&gt;` would itself get its
// `&` escaped a second time.
export function escapeHtml(text) {
  return text.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;')
}

// Trailing punctuation is trimmed off the link so a URL at the end of a
// sentence ("see https://example.com.") doesn't swallow the period into
// the href.
const URL_RE = /https?:\/\/[^\s<>"]+/g
const TRAILING_PUNCTUATION_RE = /[.,;:!?)\]}'"]+$/

// Must run after `escapeHtml`, never before: escaping markup that linkify
// had already produced would mangle the anchor tags it just wrote.
export function linkify(html) {
  return html.replace(URL_RE, (url) => {
    const trail = url.match(TRAILING_PUNCTUATION_RE)?.[0] ?? ''
    const href = trail ? url.slice(0, -trail.length) : url
    if (!href) return url
    return `<a href="${href}" rel="noopener noreferrer" target="_blank">${href}</a>${trail}`
  })
}

function render(text) {
  return linkify(escapeHtml(text))
}

// How close to the bottom still counts as "following along". 32px rather
// than 0 because a reader who nudged the wheel one line has not stopped
// following, and a strict comparison would strand them a pixel short and
// never scroll again.
const FOLLOW_SLACK = 32

// Whether new content should be scrolled into view. Pure, so the rule that
// makes a streaming answer bearable can be tested without a browser.
// Splits one SSE block ("event: ...\ndata: ...") into its two fields, or
// null when the block carries no event. Multiple `data:` lines are legal
// SSE and get joined with `\n`, though this protocol only ever sends one.
//
// Null is the common case as well as the error case: the server keeps the
// stream alive through a silent run by sending comment blocks, and a
// comment has no `event:` line. Lifted out of `start` so it can be tested —
// it touches no DOM.
export function parseFrame(block) {
  let event = null
  const dataLines = []
  for (const line of block.split('\n')) {
    if (line.startsWith('event:')) event = line.slice('event:'.length).trim()
    else if (line.startsWith('data:')) dataLines.push(line.slice('data:'.length).trim())
  }
  if (!event) return null
  return { event, data: dataLines.join('\n') }
}

export function shouldFollow(scrollTop, clientHeight, scrollHeight, slack = FOLLOW_SLACK) {
  return scrollHeight - scrollTop - clientHeight <= slack
}

// Tallest the composer may grow, in px — about five lines at this font.
// Matches the `max-height` in chat.html; if one changes the other must.
const COMPOSER_CAP = 200

export function composerHeight(scrollHeight, cap = COMPOSER_CAP) {
  return Math.min(scrollHeight, cap)
}

// How long the Delete button of a confirm-in-place stays dead after the
// confirm opens.
//
// Every one of these swaps the button for the confirm synchronously, and
// the confirm's Delete lands on the pixel the button it replaced occupied —
// so the second click of a double-click, or a double-tap, presses Delete
// without the confirm having been on screen for a frame. Measured at both
// phone and desktop widths. A window longer than any double-click interval
// (Windows tops out at 900ms for a deliberately slow setting; the usual
// threshold is 400–500ms) closes it, and one this short is invisible to
// anyone who is reading the question the confirm asks.
const CONFIRM_ARM_MS = 350

// The idle window after which an unpinned thread is deleted, and the point
// at which the sidebar starts saying so. Both mirror core: 48h expiry in
// `Core::THREAD_IDLE_SECS`, and "worth warning" at 36h.
const EXPIRES_AFTER_MS = 48 * 3600 * 1000
const WARN_AFTER_MS = 36 * 3600 * 1000

export function threadLabel(thread) {
  return thread.title ? { text: thread.title, unnamed: false } : { text: 'New thread', unnamed: true }
}

// "2h", "4d", or "expires in 12h" once an unpinned thread is close to
// going — so nobody learns about expiry by losing something.
export function whenLabel(thread, now = Date.now()) {
  // NaN propagates silently through every comparison below, so an
  // unparseable timestamp would reach the row as "NaNd". Saying nothing is
  // the honest answer: the row still has its title and its controls.
  const then = Date.parse(thread.updated_at)
  if (Number.isNaN(then)) return { text: '', expiring: false }
  const age = Math.max(0, now - then)
  if (!thread.pinned && age >= WARN_AFTER_MS) {
    const left = Math.max(1, Math.ceil((EXPIRES_AFTER_MS - age) / 3600000))
    return { text: `expires in ${left}h`, expiring: true }
  }
  const minutes = Math.floor(age / 60000)
  if (minutes < 5) return { text: 'now', expiring: false }
  if (minutes < 60) return { text: `${minutes}m`, expiring: false }
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return { text: `${hours}h`, expiring: false }
  return { text: `${Math.floor(hours / 24)}d`, expiring: false }
}

// The composer's request body. Named rather than inlined so the one place
// the thread id crosses the wire is the one place a test can hold.
export function sendBody(text, thread) {
  return JSON.stringify({ text, thread })
}

// Which thread the composer sends into after a list refresh. The page's own
// choice wins: the server's `current` is whichever thread was touched last
// anywhere — the phone, another tab, a run that just finished — and adopting
// it would send the next message into a conversation not on screen.
//
// `adopt` is for the two callers that redraw the transcript from
// `/chat/history` in the same breath: there the server's current thread and
// the one on screen are the same thing, so taking its answer is right.
export function resolveCurrent(list, shown, adopt = false) {
  if (!adopt && shown !== null && list.some((t) => t.id === shown)) return shown
  return list.find((t) => t.current)?.id ?? null
}

// Whether the thread on screen has gone from the list — expired by the 48h
// sweep while the tab slept, or deleted from the phone. `resolveCurrent`
// answers this by silently handing the composer to the server's current
// thread, which is right as far as it goes but leaves the old transcript on
// screen with nothing said. The caller asks this first so it can say so.
//
// Never true while adopting: those callers redraw from `/chat/history`, so
// the thread they are moving to *is* the server's current one. It also stops
// `vanished` — which refreshes with `adopt` — from calling itself forever.
export function threadVanished(list, shown, adopt = false) {
  return !adopt && shown !== null && !list.some((t) => t.id === shown)
}

// A stored itinerary is deliberately a display string rather than a live
// offer. Its grammar is owned by `duffel::itinerary`: an airport at either
// end and zero or more connection airports between ` ✈ ` separators. The
// middle value is a layover, while the ends carry local clock and date.
export function parseItinerary(itinerary) {
  if (typeof itinerary !== 'string' || !itinerary.trim()) return []
  return itinerary.split(/\s+✈\s+/).map((raw, index, all) => {
    const match = raw.trim().match(/^([A-Z]{3})(?:\/([A-Z]{3}))?(?:\s+(.+))?$/)
    if (!match) return { airport: raw.trim(), departsFrom: null, time: null, date: null, wait: null }
    const detail = match[3] ?? ''
    const stamped = detail.match(/^(\d{2}:\d{2})\s+(\d{2}\.\d{2})$/)
    const endpoint = index === 0 || index === all.length - 1
    return {
      airport: match[1],
      departsFrom: match[2] ?? null,
      time: endpoint && stamped ? stamped[1] : null,
      date: endpoint && stamped ? stamped[2] : null,
      wait: !endpoint && detail ? detail : null,
    }
  })
}

// The one option a flight is currently using. A sole candidate is the pick
// by elimination, matching core's readiness rule even if its `chosen` flag is
// false.
export function selectedCandidate(segment) {
  return segment?.candidates?.find((candidate) => candidate.chosen)
    ?? (segment?.candidates?.length === 1 ? segment.candidates[0] : null)
}

function localMinutes(value) {
  const match = typeof value === 'string'
    ? value.match(/^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):\d{2}$/)
    : null
  if (!match) return null
  return Date.UTC(Number(match[1]), Number(match[2]) - 1, Number(match[3]), Number(match[4]), Number(match[5])) / 60000
}

// A day as the page prints it: in full on a flight card, short on the
// timeline and on the cards for everything else. `locale` exists for the
// tests, which need a short form they can spell out; the page passes none
// and gets the reader's, so the two never disagree about a date — it is
// one formatter.
export function dateLabel(value, compact = false, locale = undefined) {
  const match = typeof value === 'string' ? value.match(/^(\d{4})-(\d{2})-(\d{2})$/) : null
  if (!match) return value || 'Date not set'
  const date = new Date(Date.UTC(Number(match[1]), Number(match[2]) - 1, Number(match[3])))
  return new Intl.DateTimeFormat(locale, compact
    ? { month: 'short', day: 'numeric', timeZone: 'UTC' }
    : { weekday: 'short', month: 'short', day: 'numeric', year: 'numeric', timeZone: 'UTC' })
    .format(date)
}

export function clockLabel(value) {
  return typeof value === 'string' && /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}/.test(value)
    ? value.slice(11, 16)
    : '—'
}

// When a non-flight item is, in the timeline's short form: a stay that
// ends on a later day is a range, an activity with a clock shows it, and
// anything else is its day. A flight card prints its date in full instead
// and leaves the clocks to its options.
export function itemDateLabel(item, locale = undefined) {
  const day = dateLabel(item.date, true, locale)
  const end = typeof item.ends_at === 'string' ? item.ends_at.slice(0, 10) : ''
  if (/^\d{4}-\d{2}-\d{2}$/.test(end) && end !== item.date) {
    return `${day} – ${dateLabel(end, true, locale)}`
  }
  const clock = clockLabel(item.starts_at)
  return clock === '—' ? day : `${day}, ${clock}`
}

export function durationLabel(minutes) {
  if (!Number.isFinite(minutes) || minutes < 0) return 'Duration unavailable'
  const days = Math.floor(minutes / (24 * 60))
  const hours = Math.floor((minutes % (24 * 60)) / 60)
  const rest = minutes % 60
  if (days > 0) {
    return [
      `${days}d`,
      hours > 0 ? `${hours}h` : '',
      rest > 0 ? `${String(rest).padStart(2, '0')}m` : '',
    ].filter(Boolean).join(' ')
  }
  if (hours === 0) return `${rest}m`
  return rest === 0 ? `${hours}h` : `${hours}h ${String(rest).padStart(2, '0')}m`
}

// Checks the join between two independently stored flights. The two
// timestamps are comparable only when they name the same airport: both clocks
// are local to that place. This is the same boundary core uses.
export function connectionCheck(before, after) {
  const arrival = selectedCandidate(before)
  const departure = selectedCandidate(after)
  if (!arrival || !departure) {
    return { tone: 'warning', text: 'Choose both flights to check this connection.' }
  }
  if (before.destination !== after.origin) {
    return {
      tone: 'warning',
      text: `Airport transfer: arrive at ${before.destination}, continue from ${after.origin}. Travel between them is not included.`,
    }
  }
  const landed = localMinutes(arrival.arriving_at_local)
  const leaves = localMinutes(departure.departing_at_local)
  if (landed === null || leaves === null) {
    return { tone: 'warning', text: `Connection at ${before.destination}: timing unavailable.` }
  }
  const minutes = leaves - landed
  if (minutes < 0) {
    return { tone: 'danger', text: `Impossible connection at ${before.destination}: the next flight leaves before arrival.` }
  }
  const wait = durationLabel(minutes)
  if (minutes < 180) {
    return { tone: 'danger', text: `${wait} at ${before.destination} — tight connection; allow at least 3 hours between separate tickets.` }
  }
  return { tone: 'ready', text: `${wait} at ${before.destination} between the selected flights.` }
}

// The flights on a trip, in order. The route, the timeline and the
// connection checks are about airports, and a stay or an activity has
// none: it sits between legs without being one.
function tripFlights(trip) {
  return (trip?.items ?? []).filter((item) => item.kind === 'flight')
}

export function tripTimelinePoints(trip) {
  const flights = tripFlights(trip)
  if (!flights.length) return []
  const points = [{ code: flights[0].origin, date: flights[0].date, gap: false }]
  for (let i = 0; i < flights.length; i++) {
    const segment = flights[i]
    const chosen = selectedCandidate(segment)
    const stops = chosen ? parseItinerary(chosen.itinerary).slice(1, -1) : []
    for (const stop of stops) {
      points.push({
        code: stop.departsFrom ? `${stop.airport}/${stop.departsFrom}` : stop.airport,
        date: stop.wait ?? 'Connection',
        gap: false,
      })
    }
    points.push({ code: segment.destination, date: '', gap: false })
    const next = flights[i + 1]
    if (next && next.origin !== segment.destination) {
      points[points.length - 1].gap = true
      points.push({ code: next.origin, date: next.date, gap: false })
    } else if (next) {
      points[points.length - 1].date = next.date
    }
  }
  return points
}

// The airports in order, as the list row and the eyebrow print them.
export function tripRoute(trip) {
  const flights = tripFlights(trip)
  if (!flights.length) return 'No route yet'
  const codes = [flights[0].origin]
  for (const flight of flights) {
    if (codes[codes.length - 1] !== flight.origin) codes.push(flight.origin)
    codes.push(flight.destination)
  }
  return codes.join(' → ')
}

export function tripLoadIsCurrent(request, current, choicePending) {
  return request === current && !choicePending
}

export function savedFareQualifier(source) {
  const from = String(source).toLowerCase()
  // A fare off a forwarded confirmation is not a quote that may have
  // moved since: the preamble defines it as the total actually paid, and
  // "when saved" would hedge a number there is nothing tentative about.
  if (from === 'email') return { prefix: '', note: 'paid' }
  return from === 'ignav'
    ? { prefix: 'from ', note: 'estimate when saved' }
    : { prefix: '', note: 'when saved' }
}

// The mark a booked item carries: the confirmation code where there is
// one, and otherwise the fact of it. One helper because a flight and
// everything else say it the same way, and a card that said it two ways
// would read as two different states.
export function bookedMark(item) {
  return item.confirmation_code ? `booked · ${item.confirmation_code}` : 'booked'
}

// What a flight card says where its options would be when it has none.
//
// A booked leg with no option is a ticket the reader holds whose flight
// the confirmation did not name — offering to search that route reads as
// though the booking never arrived, which is the complaint this answers.
// An unbooked leg is a route waiting for a search, and says so.
export function noFlightLine(item) {
  return item.booked
    ? 'Booked. The confirmation did not say which flight.'
    : 'No flight saved yet. Ask Scout in chat to search this route.'
}

export function tripPdfFilename(name) {
  const stem = String(name).toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-|-$/g, '')
    .slice(0, 60)
    .replace(/-$/, '')
  return `${stem || 'trip'}-itinerary.pdf`
}

// The DELETE body for `/chat/trips/segment`. Removing an item renumbers the
// ones after it — removing item 1 shifts item 2 down to 1, and a flight's
// parked options with it — so a tab holding a trip it drew a while ago
// could ask to delete "item 2" when item 2 is no longer the thing it drew.
// `position` alone gives the server nothing to check that against, which
// is why the body also carries what the card showed: a flight its route,
// anything else its title, and every item its date. What a card has no use
// for is `null`, not an omitted key, matching `RemoveItemIn` on the Rust
// side, whose `Option<String>`s read a JSON `null` as "nothing to verify"
// rather than as a value.
export function removeItemBody(tripName, item) {
  const flight = item.kind === 'flight'
  return JSON.stringify({
    trip: tripName,
    position: item.position,
    origin: flight ? item.origin ?? null : null,
    destination: flight ? item.destination ?? null : null,
    title: flight ? null : item.title ?? null,
    date: item.date ?? null,
  })
}

// The body for `/chat/trips/keep`. A name is everything the route needs —
// unlike `removeItemBody` it carries nothing for the server to check a stale
// tab against, because keeping is idempotent: `keep_trip` succeeds again on
// a trip this account already kept, so there is no stale state for a second
// press to collide with.
export function keepBody(tripName) {
  return JSON.stringify({ trip: tripName })
}

// The DELETE body for `/chat/trips` — the name and nothing else, on
// purpose. That route is one path segment short of `/chat/trips/segment`
// and takes the same method, so the two bodies are the thing keeping "drop
// this leg" and "drop the whole itinerary" apart on the wire. `DeleteTripIn`
// is `deny_unknown_fields` on the Rust side: a body with `position` in it —
// which is to say anything `removeItemBody` built — is refused there rather
// than read as a whole-trip delete. This carries no `position` for that
// refusal to be about.
export function deleteTripBody(tripName) {
  return JSON.stringify({ trip: tripName })
}

// What the traveller is about to lose, counted, for the confirm to say out
// loud. A trip delete takes every item and every parked flight option with
// it — see `Store::delete_trip` — and "Delete this trip?" on its own gives
// the reader no way to tell a stray press on an empty draft from one that
// throws away an afternoon of price research.
export function tripDeleteConsequence(trip) {
  const items = trip?.items ?? []
  const options = items.reduce((total, item) => total + (item.candidates?.length ?? 0), 0)
  if (!items.length) return 'Nothing is saved on it yet.'
  const count = `${items.length} ${items.length === 1 ? 'item' : 'items'}`
  if (!options) return `Its ${count} go${items.length === 1 ? 'es' : ''} with it.`
  return `Its ${count} and ${options} saved flight ${options === 1 ? 'option' : 'options'} go with it.`
}

// Where a message typed on the Trips tab should go, and what the composer
// says about it. `direct` is the one scope the web client may ever post
// into — the thread it shares with Telegram 1:1 chat. Anything else is a
// Telegram group: other people are in it, and a reply typed here must not
// land there as though the traveller said it in the room. That case and an
// orphaned trip (its owning chat gone) both resolve to `thread: null` — a
// fresh thread the composer starts before sending, which is how an orphan
// gets a chat again without ever taking a group's away from it.
export function composerTarget(trip) {
  // No trip on screen — the empty state, or a selection not yet resolved.
  // Nothing to name, so the line has nothing to say, but the send still
  // gets a thread of its own rather than reusing whatever the page had
  // open in Chat before Trips was opened.
  if (!trip) return { thread: null, label: '' }
  const chat = trip.chat
  if (!chat) return { thread: null, label: 'to a new chat' }
  if (chat.scope !== 'direct') {
    return { thread: null, label: 'planned in a Telegram group — replies go to a new chat' }
  }
  return { thread: chat.id, label: chat.title ? `to "${chat.title}"` : 'to an unnamed thread' }
}

// The trip's timeline with the mail that has arrived for it laid in. An
// arrival waits as a dashed row where its booking would sit, so the
// reader sees the trip as it will be once they say Add — not a separate
// inbox to cross-reference against the timeline. Only what is still
// undecided and was actually read as a booking gets a row; the rest is
// "Other mail".
//
// Matched to the trip by name, not id: a Plan on `/chat/trips` carries no
// id — trips are named everywhere on this page (`keepBody`, `removeItemBody`)
// — and the server puts the same `trip_name` on the arrival. An arrival's
// `trip_id` is the mail side's key and plays no part here.
//
// Ordered by date, a pending row after the items of its day: the items
// already there are what the reader chose, and something that just arrived
// waits behind them rather than pushing in front. Pendings on one day keep
// their id order, which is arrival order — a stable read on every repaint.
export function pendingRowsFor(trip, arrivals) {
  const rows = trip.items.map((item) => ({ kind: 'item', item }))
  const mine = (arrivals ?? [])
    .filter((a) => a.status === 'pending' && a.booking && a.trip_name === trip.name)
    .sort((a, b) => a.id - b.id)
  for (const arrival of mine) {
    // The first row on a later day is where this one goes; none means the
    // end. `>` rather than `>=` is the "after same-day items" rule. The
    // server guarantees a date on anything it read as a booking; the
    // fallback only keeps a malformed row from throwing, at the front.
    let at = rows.findIndex((row) => rowDate(row) > (arrival.date ?? ''))
    if (at < 0) at = rows.length
    rows.splice(at, 0, { kind: 'pending', arrival })
  }
  return rows
}

function rowDate(row) {
  return (row.kind === 'item' ? row.item.date : row.arrival.date) ?? ''
}

// One line per mail that did not become a pending row: what the reader
// needs to recognise it (who, what, when) and why it is here rather than
// on a timeline. `sender` is the display name when the address carried
// one — "TAP" is recognisable where "news@flytap.com" is a squint — and
// `address` is always the bare address, shown beside it: a display name
// is whatever the sender typed, and a mail calling itself "Booking.com"
// from elsewhere must not be the only thing on screen. `locale` exists
// for the tests, as on `dateLabel`; the page passes none.
const MAIL_REASONS = {
  not_booking: 'not a booking',
  failed: 'could not read',
  ignored: 'ignored',
}

export function otherMailLines(rows, locale = undefined) {
  return (rows ?? []).map((row) => ({
    mail_id: row.mail_id,
    sender: sender(row.from).name,
    address: sender(row.from).address,
    subject: row.subject?.trim() ? row.subject : '(no subject)',
    when: dateLabel(String(row.received_at ?? '').slice(0, 10), true, locale),
    reason: MAIL_REASONS[row.reason] ?? row.reason ?? '',
    note: row.forwarded ? 'forwarded to you' : 'not forwarded',
    attachments: row.attachments ?? [],
    // The one reason that gets a button; kept as a flag so the page does
    // not compare against its own display text.
    failed: row.reason === 'failed',
  }))
}

// What the × on a row is called. A screen reader announces a button out
// of its row's context, and a list of identical "Delete"s is a list of
// buttons with nothing behind them — so the label carries the two things
// the row is recognised by. Built from a line of `otherMailLines`, whose
// `subject` is never blank.
export function otherMailDeleteLabel(line) {
  return `Delete mail from ${line.sender}: ${line.subject}`
}

// `Name <addr>`, `"Name, quoted" <addr>`, `<addr>` or a bare address:
// the name is the part before the brackets, unquoted, and the address is
// the part inside them — or the whole thing when there are none. A name
// that is empty falls back to the address, so `sender` is never blank.
function sender(from) {
  const text = String(from ?? '').trim()
  const match = text.match(/^"?([^"<]*?)"?\s*<([^>]*)>$/)
  if (!match) return { name: text, address: text }
  const address = match[2].trim()
  return { name: match[1].trim() || address, address }
}

// Mirrors `scout_core::inbox::normalise_handle` — the same checks, in the
// same order, with the server's own sentences — so the form can say what
// is wrong as the reader types instead of after a round trip. The reserved
// list is deliberately not copied: it is the server's to keep, and the
// live check asks it. `null` means nothing to say.
export function handleProblem(raw) {
  const trimmed = String(raw ?? '').trim()
  // Checked before lowercasing: JS folds the Kelvin sign to an ASCII k
  // where the server, lowercasing bytes, sees something it refuses.
  if (!/^[A-Za-z0-9.]*$/.test(trimmed)) return 'letters, digits and dots only'
  const h = trimmed.toLowerCase()
  if (h.length < 3 || h.length > 30) return 'a handle is 3 to 30 characters'
  if (h.startsWith('.') || h.endsWith('.')) return 'a handle cannot start or end with a dot'
  return null
}

// The trace panel's model. Pure so it can be tested without a DOM, and
// shared by the saved trace (rows from the server) and the live one (rows
// built from frames), which must read identically.
const ARGS_LINE_CAP = 120

export function traceDuration(ms) {
  if (typeof ms !== 'number') return ''
  if (ms < 60000) return `${(ms / 1000).toFixed(1)}s`
  const m = Math.floor(ms / 60000)
  const s = Math.round((ms % 60000) / 1000)
  return `${m}m ${String(s).padStart(2, '0')}s`
}

function argsLine(args) {
  const text = args === undefined || args === null ? '' : JSON.stringify(args)
  return text.length > ARGS_LINE_CAP ? text.slice(0, ARGS_LINE_CAP) + '…' : text
}

export function traceLines(run, rows) {
  let head = ''
  if (run && run.outcome) {
    head = [run.outcome, run.ended_at && run.started_at ? traceDuration(Date.parse(run.ended_at) - Date.parse(run.started_at)) : '', run.detail]
      .filter(Boolean).join(' · ')
  } else if (run && run.id && !run.outcome && run.started_at) {
    // A saved run that never closed its row: the process died mid-run.
    head = 'unfinished'
  }
  return {
    head,
    rows: rows.map(r => ({
      seq: r.seq,
      kind: r.kind,
      nested: Boolean(r.nested),
      status: r.status,
      label: r.kind === 'tool' ? `${r.tool} ${argsLine(r.args)}`.trimEnd() : (r.detail || ''),
      duration: r.kind === 'tool' ? traceDuration(r.duration_ms) : '',
      detail: r.detail || '',
      result: r.result,
      truncated: Boolean(r.truncated),
      args: r.args,
    })),
  }
}

// Folds a live frame into the rows a saved trace would have had.
export function applyTraceFrame(rows, frame) {
  if (frame.kind === 'started') {
    return [...rows, { seq: frame.seq, kind: 'tool', tool: frame.tool, args: frame.args, nested: frame.nested }]
  }
  if (frame.kind === 'finished') {
    return rows.map(r => r.seq === frame.seq
      ? { ...r, duration_ms: frame.duration_ms, status: frame.status, detail: frame.detail || undefined }
      : r)
  }
  if (frame.kind === 'event') {
    return [...rows, { seq: frame.seq, kind: 'event', detail: frame.detail, status: frame.error ? 'failed' : 'ok' }]
  }
  return rows
}

// The server's `debug_command`, mirrored: the word alone, or with `on` or
// `off`, and nothing else. Case-sensitive like Telegram's slash commands,
// so `/Debug` in prose is prose.
export function isDebugCommand(text) {
  const words = text.split(/\s+/).filter(Boolean)
  if (words[0] !== '/debug') return false
  return words.length === 1 || (words.length === 2 && (words[1] === 'on' || words[1] === 'off'))
}

// Whether a run that produced no answer keeps its bubble anyway. A failed
// run writes no history turn, so with debug on the live trace under the
// bubble is that trace's only copy — and an empty bubble is what keeps it
// on the page. Nothing traced, or debug off, and the bubble goes as ever.
export function keepFailedTurn(end, debugOn, rowCount) {
  return (end.status === 'error' || end.status === 'busy') && debugOn && rowCount > 0
}

function start() {
  const csrfToken = document.querySelector('meta[name="csrf"]').content
  const turnsEl = document.getElementById('turns')
  const statusEl = document.getElementById('status')
  const noticeEl = document.getElementById('notice')
  const composeTargetEl = document.getElementById('compose-target')
  const askForm = document.getElementById('ask')
  const textEl = document.getElementById('text')
  const sendButton = document.getElementById('send')
  const resetForm = document.getElementById('reset')
  const mirrorButton = document.getElementById('mirror')
  const sideEl = document.getElementById('side')
  const threadsEl = document.getElementById('threads')
  const menuButton = document.getElementById('menu')
  const chatWorkspace = document.getElementById('chat-workspace')
  const tripsView = document.getElementById('trips-view')
  const chatTab = document.getElementById('view-chat')
  const tripsTab = document.getElementById('view-trips')
  const tripCount = document.getElementById('trip-count')
  const tripList = document.getElementById('trip-list')
  const tripDetail = document.getElementById('trip-detail')
  const otherMailEl = document.getElementById('other-mail')
  const handleLineEl = document.getElementById('handle-line')
  // The thread the page is showing. Every message names it, so a thread
  // the phone started meanwhile cannot swallow a message meant for this one.
  //
  // Set in four places and no others: `loadHistory`, `openThread`,
  // `newThread` and `vanished` — every one of which puts that thread's
  // transcript on screen in the same breath. A list refresh is not one of
  // them: see `resolveCurrent`.
  let currentThread = null
  // Two list refreshes can be in flight at once — a run finishing while the
  // tab wakes up — and the older answer describes a list that has since
  // moved on. Same for two rows tapped in quick succession.
  let refreshSeq = 0
  let openSeq = 0
  // The threads the rows on screen were built from, held so the minute
  // ticker below can re-label them without asking the server again.
  let lastList = []
  let trips = []
  let currentTrip = null
  let tripsLoaded = false
  let tripLoadSeq = 0
  let tripChoicePending = false
  // The booking inbox as `/chat/inbox` last described it: `null` until the
  // route has answered once, and left alone when a refresh fails — a
  // stale inbox is the pending rows the reader already saw, a cleared one
  // is those rows vanishing for no reason. Stays `null` for good when the
  // route is not there (the feature off), and then nothing of it is drawn:
  // the Trips tab is exactly what it was before.
  let inbox = null
  // Set by Change on a saved address and cleared by Save or Cancel, so the
  // form can stand in for the address line without forgetting the handle.
  let handleEditing = false

  // Enter sends, Shift+Enter is a newline. `requestSubmit` rather than
  // `submit` because it runs the form's own validation — so Enter on an
  // empty box does nothing, which is the guard the button already relied
  // on via `required`.
  textEl.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      askForm.requestSubmit()
    }
  })

  // Grow to fit, then stop. Reset to `auto` first or `scrollHeight` keeps
  // reporting the height already set and the box only ever grows.
  function fitComposer() {
    textEl.style.height = 'auto'
    textEl.style.height = `${composerHeight(textEl.scrollHeight)}px`
  }
  textEl.addEventListener('input', fitComposer)

  // Decide *before* appending, act after: once the new content is in the
  // DOM the reader's position looks different and the question cannot be
  // asked honestly any more.
  function following() {
    return shouldFollow(turnsEl.scrollTop, turnsEl.clientHeight, turnsEl.scrollHeight)
  }
  function follow(wasFollowing) {
    if (wasFollowing) turnsEl.scrollTop = turnsEl.scrollHeight
  }

  // The words go into a child of their own rather than the `li` itself:
  // `renderAnswer` redraws them on every token, and a trace panel drawn
  // under a live answer has to survive that. Saved turns take the same
  // shape so the two paths cannot drift apart.
  function turnElement(role, text, runId) {
    const li = document.createElement('li')
    li.className = role === 'You' ? 'you' : 'scout'
    const words = node('div', 'text')
    words.innerHTML = render(text)
    li.append(words)
    // The run is remembered on the element, not only acted on, so a turn
    // already on screen can get its button when `/debug on` arrives.
    if (role !== 'You' && typeof runId === 'number') {
      li.dataset.runId = String(runId)
      if (debugOn) li.append(traceButton(li, runId))
    }
    return li
  }

  // Whether this reader has `/debug on`. Asked of the server rather than
  // remembered here: the switch is per account and outlives the page.
  let debugOn = false

  async function refreshDebug() {
    try {
      const res = await fetch('/chat/debug')
      if (res.ok) debugOn = Boolean((await res.json()).on)
    } catch {
      // A dead network leaves the switch as it was; the next send asks again.
    }
    syncTraceButtons()
  }

  // Gives every Scout turn on screen the button the flag now calls for,
  // and takes it — and any open panel — away when the flag went off. In
  // place rather than by redrawing from history: the `/debug on` exchange
  // that flipped the flag is never written to history, and a redraw would
  // take the confirmation off the screen the moment it appeared.
  function syncTraceButtons() {
    for (const li of turnsEl.querySelectorAll('li[data-run-id]')) {
      const btn = li.querySelector('.trace-btn')
      if (debugOn && !btn) {
        // Before the panel, if one is open, so the order matches a saved
        // turn's: words, button, trace.
        li.insertBefore(traceButton(li, Number(li.dataset.runId)), li.querySelector('.trace'))
      } else if (!debugOn && btn) {
        btn.remove()
        li.querySelector('.trace')?.remove()
      }
    }
  }

  function traceButton(li, runId) {
    const btn = node('button', 'trace-btn', 'Trace')
    btn.type = 'button'
    btn.dataset.runId = String(runId)
    btn.addEventListener('click', () => { openTrace(li, runId).catch(() => {}) })
    return btn
  }

  // Draws the trace under a turn from `traceLines`, replacing whatever
  // panel was there. Everything goes through `textContent`: arguments and
  // results are the tools' own text — a shop's page title, a provider's
  // error body — and none of it is to be trusted as markup.
  function renderTracePanel(li, run, rows) {
    // Redrawn wholesale on every live frame, so what the reader did to the
    // panel already there — scrolled, opened a row — is carried over by
    // hand, the way `showStatus` keeps its place in the status box.
    const old = li.querySelector('.trace')
    const wasFollowing = old ? shouldFollow(old.scrollTop, old.clientHeight, old.scrollHeight) : true
    const scrollTop = old ? old.scrollTop : 0
    const opened = new Set([...(old ? old.querySelectorAll('.row.open') : [])].map((row) => Number(row.dataset.seq)))
    const lines = traceLines(run, rows)
    const panel = node('div', 'trace')
    if (lines.head) panel.append(node('div', 'head', lines.head))
    for (const line of lines.rows) {
      const row = node('div', `row ${line.kind}`)
      if (line.nested) row.classList.add('nested')
      if (line.status === 'failed') row.classList.add('failed')
      row.append(node('span', 'label', line.label))
      panel.append(row)
      // An event is one sentence and carries its status in its colour; a
      // tool call has a duration, a chip, and something to open.
      if (line.kind !== 'tool') continue
      row.append(node('span', 'dur', line.duration))
      const status = line.status === undefined ? 'running' : line.status
      row.append(node('span', `chip ${status}`, status))
      row.dataset.seq = String(line.seq)
      row.tabIndex = 0
      row.setAttribute('role', 'button')
      const toggle = () => {
        if (row.classList.toggle('open')) row.after(node('pre', '', traceDetail(line)))
        else row.nextElementSibling?.remove()
      }
      row.addEventListener('click', toggle)
      row.addEventListener('keydown', (e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault()
          toggle()
        }
      })
      if (opened.has(line.seq)) toggle()
    }
    old?.remove()
    li.append(panel)
    // After the append: a panel not yet laid out has no height to scroll.
    panel.scrollTop = wasFollowing ? panel.scrollHeight : scrollTop
  }

  // What a tool row opens to: the arguments, a blank line, then the result
  // — pretty-printed when it is JSON, as it came otherwise. A live row has
  // no result yet, so a failed one shows the error it was closed with.
  function traceDetail(line) {
    let text = JSON.stringify(line.args === undefined ? null : line.args, null, 2)
    const result = line.result === undefined || line.result === null ? line.detail : line.result
    if (result) {
      let shown = result
      try {
        shown = JSON.stringify(JSON.parse(result), null, 2)
      } catch {
        // Not JSON: the raw text is the result.
      }
      text += `\n\n${shown}`
    }
    if (line.truncated) text += "\n… (cut at the store's cap)"
    return text
  }

  // Toggles the saved trace under a turn. Debug off is a notice rather
  // than a panel — the button should not have been there, and the reader
  // is told how to get one; a run the store no longer keeps is a panel
  // that says so, because the reader asked and silence would read as a
  // broken button.
  async function openTrace(li, runId) {
    const open = li.querySelector('.trace')
    if (open) {
      open.remove()
      return
    }
    let res
    try {
      res = await fetch(`/chat/runs/${runId}/trace`)
    } catch {
      showNotice('Could not load the trace. Try again.')
      return
    }
    if (res.status === 403) {
      showNotice('Turn debug on with /debug on to see traces.')
      return
    }
    if (res.status === 404) {
      const panel = node('div', 'trace')
      panel.append(node('div', 'head', 'trace no longer kept'))
      li.append(panel)
      return
    }
    if (!res.ok) {
      showNotice('Could not load the trace. Try again.')
      return
    }
    const { run, rows } = await res.json()
    renderTracePanel(li, run, rows)
  }

  // The status box is capped at a quarter of the viewport, so on a long
  // run the newest reasoning is the part below the fold. Tail it — but ask
  // the same question `.turns` asks, and for the same reason: a reader who
  // scrolled up to read something should not be dragged back down by the
  // next token. While hidden every measurement is 0, which reads as "at the
  // bottom", so the first status of a run always follows.
  function showStatus(text) {
    const wasFollowing = shouldFollow(
      statusEl.scrollTop, statusEl.clientHeight, statusEl.scrollHeight)
    statusEl.textContent = text
    statusEl.hidden = false
    if (wasFollowing) statusEl.scrollTop = statusEl.scrollHeight
    statusEl.classList.toggle('long', statusEl.scrollHeight > statusEl.clientHeight)
  }

  function hideStatus() {
    statusEl.hidden = true
    statusEl.textContent = ''
    statusEl.classList.remove('long')
  }

  function showNotice(text) {
    noticeEl.textContent = text
    noticeEl.hidden = false
  }

  function hideNotice() {
    noticeEl.hidden = true
    noticeEl.textContent = ''
  }

  // Redraws the line above the composer from `composerTarget`. Called from
  // both the places that can change its answer — `switchView`, because the
  // Chat tab has no trip to name, and `renderTripDetail`, which runs on
  // every path that changes which trip is selected (a load, a click in the
  // list, a saved choice). Missing either call would leave the line naming
  // the trip that was picked before the reader last acted.
  function updateComposeTarget() {
    if (tripsView.hidden) {
      composeTargetEl.hidden = true
      composeTargetEl.textContent = ''
      return
    }
    const target = composerTarget(trips.find((trip) => trip.name === currentTrip))
    // An empty label means there is nothing to name yet — no trips loaded,
    // or a selection not yet resolved — so the line says nothing rather
    // than something misleading.
    composeTargetEl.hidden = !target.label
    composeTargetEl.textContent = target.label ? `↩ ${target.label}` : ''
  }

  function node(tag, className, text) {
    const el = document.createElement(tag)
    if (className) el.className = className
    if (text !== undefined) el.textContent = text
    return el
  }

  function moneyLabel(price, currency) {
    if (!Number.isFinite(price)) return 'Price unavailable'
    if (!currency) return price.toFixed(2)
    try {
      return new Intl.NumberFormat(undefined, {
        style: 'currency', currency, minimumFractionDigits: 0, maximumFractionDigits: 2,
      }).format(price)
    } catch {
      return `${price.toFixed(2)} ${currency}`
    }
  }

  // Shown wherever a draft is marked — the list row and the trip's own
  // header. "Draft" alone was tried in production and told the traveller
  // nothing about what would happen to it; this says the actual
  // consequence, which is the only thing worth a glance answering.
  const DRAFT_NOTE = 'Clears with its chat unless kept'

  function switchView(view) {
    const showingTrips = view === 'trips'
    chatWorkspace.hidden = showingTrips
    tripsView.hidden = !showingTrips
    chatTab.setAttribute('aria-pressed', String(!showingTrips))
    tripsTab.setAttribute('aria-pressed', String(showingTrips))
    menuButton.hidden = showingTrips
    if (mirrorButton) mirrorButton.hidden = showingTrips
    if (showingTrips && !tripsLoaded) loadTrips().catch(() => {})
    // Switching tabs is the other path (besides picking a trip) that
    // changes what the line above the composer should say — a load
    // already in flight will say it again once `renderTripDetail` runs.
    updateComposeTarget()
  }

  async function loadTrips() {
    // A selection response carries the authoritative post-write trip. Do not
    // start a read that could race it and repaint an older snapshot.
    if (tripChoicePending) return
    const seq = ++tripLoadSeq
    tripDetail.setAttribute('aria-busy', 'true')
    // Asked for in the same breath as the trips, not after them: the
    // pending rows are drawn into the timeline, and a second repaint once
    // the inbox arrived would show the trip twice — once without the mail
    // that is waiting for it, then with. `fetchInbox` never rejects, so an
    // inbox that fails leaves the trips loading exactly as before.
    const inboxReq = fetchInbox()
    try {
      const res = await fetch('/chat/trips')
      if (!res.ok) throw new Error('refused')
      const loaded = await res.json()
      const loadedInbox = await inboxReq
      // Two loads can overlap when a hidden tab wakes as Trips is opened.
      // Only the latest response may redraw the page.
      if (!tripLoadIsCurrent(seq, tripLoadSeq, tripChoicePending)) return
      trips = loaded
      if (loadedInbox) inbox = loadedInbox
      tripsLoaded = true
      tripCount.textContent = String(trips.length)
      tripCount.hidden = trips.length === 0
      if (!trips.some((trip) => trip.name === currentTrip)) currentTrip = trips[0]?.name ?? null
      renderTripList()
      renderTripDetail()
      renderInboxSide()
    } catch {
      if (!tripLoadIsCurrent(seq, tripLoadSeq, tripChoicePending)) return
      showTripEmpty(
        'Could not load your trips',
        'Return to chat or reload the page to try again.',
      )
    } finally {
      if (seq === tripLoadSeq) tripDetail.removeAttribute('aria-busy')
    }
  }

  function showTripEmpty(title, copy) {
    tripDetail.replaceChildren()
    const empty = node('div', 'trip-empty')
    const inner = node('div')
    inner.innerHTML = '<svg viewBox="0 0 64 64" width="52" height="52" aria-hidden="true"><path d="M10 43h44M16 37l11-20 7 3-3 14 14-9 5 4-18 13z" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round"/></svg>'
    inner.append(node('h2', '', title), node('p', '', copy))
    empty.append(inner)
    tripDetail.append(empty)
  }

  // The inbox, or `null` on any failure — a 404 (the feature is off), a
  // 500, a dead network — so a caller keeps what it had. Never throws.
  async function fetchInbox() {
    try {
      const res = await fetch('/chat/inbox')
      if (!res.ok) return null
      return await res.json()
    } catch {
      return null
    }
  }

  function renderInboxSide() {
    renderOtherMail()
    renderHandle()
  }

  function renderTripList() {
    tripList.replaceChildren()
    for (const trip of trips) {
      const li = node('li')
      const button = node('button')
      button.type = 'button'
      button.setAttribute('aria-current', String(trip.name === currentTrip))
      button.append(
        node('span', 'trip-list-name', trip.name),
        node('span', 'trip-list-route', tripRoute(trip)),
      )
      // Drafts were hidden here for a day and it read as the feature not
      // working — see `scout_core::trips::list`. Shown again, but marked:
      // an empty tab and a tab full of trips nobody asked to keep look the
      // same at a glance without this.
      if (!trip.kept) button.append(node('span', 'trip-list-draft', DRAFT_NOTE))
      button.addEventListener('click', () => {
        currentTrip = trip.name
        renderTripList()
        renderTripDetail()
        tripDetail.scrollTop = 0
      })
      li.append(button)
      tripList.append(li)
    }
  }

  function renderOverview(trip) {
    const card = node('section', 'trip-overview')
    const label = node('div', 'trip-overview-label')
    label.append(node('span', '', 'Trip timeline'), node('span', '', `${trip.items.length} ${trip.items.length === 1 ? 'item' : 'items'}`))
    const timeline = node('div', 'trip-timeline')
    for (const point of tripTimelinePoints(trip)) {
      const item = node('div', point.gap ? 'trip-point gap' : 'trip-point')
      item.append(
        node('span', 'point-dot'),
        node('span', 'point-code', point.code),
        node('span', 'point-date', point.date ? dateLabel(point.date, true) : ''),
      )
      timeline.append(item)
    }
    card.append(label, timeline)
    return card
  }

  function stopLabel(candidate) {
    const points = parseItinerary(candidate.itinerary)
    const stops = points.slice(1, -1)
    if (!stops.length) return { text: 'Direct', direct: true }
    const detail = stops.map((stop) => {
      const airport = stop.departsFrom ? `${stop.airport}→${stop.departsFrom}` : stop.airport
      return stop.wait ? `${stop.wait} at ${airport}` : airport
    }).join(' · ')
    return { text: `${stops.length} ${stops.length === 1 ? 'stop' : 'stops'} · ${detail}`, direct: false }
  }

  function renderOption(trip, segment, candidate) {
    const chosen = selectedCandidate(segment)?.candidate === candidate.candidate
    const label = node('label', chosen ? 'flight-option selected' : 'flight-option')
    const input = document.createElement('input')
    input.type = 'radio'
    input.name = `segment-${segment.position}`
    input.checked = chosen
    input.dataset.candidate = String(candidate.candidate)
    input.setAttribute('aria-label', `Choose option ${candidate.candidate}, ${candidate.airline}`)
    const radio = node('span', 'option-radio')

    const main = node('span', 'option-main')
    const top = node('span', 'option-top')
    top.append(
      node('span', 'airline', candidate.airline),
      node('span', 'flight-number', candidate.flight_numbers.replaceAll(',', ' · ')),
    )
    if (candidate.source) top.append(node('span', 'source-label', candidate.source))

    const line = node('span', 'flight-line')
    const depart = node('span', 'flight-time')
    depart.append(node('strong', '', clockLabel(candidate.departing_at_local)), node('span', '', segment.origin))
    const track = node('span', 'flight-track')
    const stops = stopLabel(candidate)
    const meta = node('span', 'flight-meta')
    meta.append(
      node('span', '', durationLabel(candidate.duration_minutes)),
      node('span', stops.direct ? 'stop-pill direct' : 'stop-pill', stops.text),
    )
    track.append(node('span', 'track-rule'), meta)
    const arrive = node('span', 'flight-time')
    arrive.append(node('strong', '', clockLabel(candidate.arriving_at_local)), node('span', '', segment.destination))
    line.append(depart, track, arrive)
    main.append(top, line)

    const price = node('span', 'option-price')
    const qualifier = savedFareQualifier(candidate.source)
    price.append(
      node('strong', '', qualifier.prefix + moneyLabel(candidate.quoted_price, candidate.quoted_currency)),
      node('span', '', qualifier.note),
    )
    label.append(input, radio, main, price)
    label.addEventListener('click', (event) => {
      event.preventDefault()
      if (!chosen) chooseFlight(trip, segment, candidate, label).catch(() => {})
    })
    return label
  }

  // "Stay", "Activity", "Transport": the kicker on a non-flight card, where
  // a flight's says "Segment N".
  function kindLabel(item) {
    return item.kind.charAt(0).toUpperCase() + item.kind.slice(1)
  }

  // What a sentence about an item calls it. A flight has always been a
  // "leg" on this page; the rest are what their kind says.
  function itemNoun(item) {
    return item.kind === 'flight' ? 'leg' : item.kind
  }

  function itemName(item) {
    return item.kind === 'flight' ? `${item.origin} → ${item.destination}` : item.title
  }

  // Holds the destructive half of a confirm-in-place dead until the gesture
  // that opened it is over — see `CONFIRM_ARM_MS` for the hazard. Setting
  // `disabled` on a node that has since been replaced does nothing, so an
  // opened-and-cancelled confirm needs no cleanup.
  function armConfirm(button) {
    button.disabled = true
    setTimeout(() => { button.disabled = false }, CONFIRM_ARM_MS)
  }

  // The plain "Remove" button an item starts with. Kept as its own
  // function so `removeConfirmRow`'s Cancel can rebuild exactly this and put
  // the card back the way it found it.
  function segmentRemoveButton(trip, item, slot) {
    const button = document.createElement('button')
    button.type = 'button'
    button.className = 'segment-remove-button'
    button.textContent = 'Remove'
    button.setAttribute(
      'aria-label',
      item.kind === 'flight'
        ? `Remove segment ${item.position}, ${item.origin} to ${item.destination}`
        : `Remove ${item.kind}, ${item.title}`,
    )
    button.addEventListener('click', () => {
      slot.replaceChildren(removeConfirmRow(trip, item, slot))
    })
    return button
  }

  // A second click, not `window.confirm`: a dialog blocks the whole page for
  // one item on one card, and this is destructive enough to ask about but
  // not rare enough to justify that. The row swaps in over the button it
  // replaced and swaps back on Cancel or on a failed request — only a
  // response that actually rewrote the trip (200, or the reload a 409
  // triggers) is allowed to leave it gone for good, via the full repaint
  // those paths already do.
  function removeConfirmRow(trip, item, slot) {
    const row = node('span', 'segment-remove-confirm')
    row.append(node('span', '', `Remove this ${itemNoun(item)}?`))
    const cancel = document.createElement('button')
    cancel.type = 'button'
    cancel.textContent = 'Cancel'
    cancel.addEventListener('click', () => {
      slot.replaceChildren(segmentRemoveButton(trip, item, slot))
    })
    const confirm = document.createElement('button')
    confirm.type = 'button'
    confirm.className = 'danger'
    confirm.textContent = 'Remove'
    // A double-click on Remove would otherwise take the leg on one gesture,
    // the confirm never seen.
    armConfirm(confirm)
    confirm.addEventListener('click', () => {
      confirm.disabled = true
      cancel.disabled = true
      removeItem(trip, item).then((restore) => {
        if (restore) slot.replaceChildren(segmentRemoveButton(trip, item, slot))
      }).catch(() => {
        slot.replaceChildren(segmentRemoveButton(trip, item, slot))
      })
    })
    row.append(cancel, confirm)
    return row
  }

  // A stay, an activity or a transport: one card, nothing to choose on it.
  // Its date line follows `itemDateLabel` — a range or a clock where the
  // item has one — where a flight's shows the day in full and leaves the
  // clocks to its options.
  function renderOtherItem(trip, item) {
    const card = node('article', 'item-card')
    const head = node('header', 'segment-head')
    const about = node('div')
    about.append(
      node('p', 'segment-kicker', kindLabel(item)),
      node('h3', 'item-title', item.title),
    )
    if (item.place) about.append(node('p', 'item-place', item.place))
    if (item.booked) {
      about.append(node('span', 'item-booked', bookedMark(item)))
    }
    const actions = node('div', 'segment-head-actions')
    actions.append(node('time', 'segment-date', itemDateLabel(item)))
    const removeSlot = node('span', 'segment-remove')
    removeSlot.append(segmentRemoveButton(trip, item, removeSlot))
    actions.append(removeSlot)
    head.append(about, actions)
    card.append(head)
    return card
  }

  function renderItem(trip, segment) {
    if (segment.kind !== 'flight') return renderOtherItem(trip, segment)
    const card = node('article', 'segment-card')
    const head = node('header', 'segment-head')
    const route = node('div')
    route.append(
      node('p', 'segment-kicker', `Segment ${segment.position}`),
      node('h3', 'segment-route'),
    )
    route.lastChild.append(
      document.createTextNode(segment.origin),
      node('span', 'route-arrow', '→'),
      document.createTextNode(segment.destination),
    )
    // The same mark a stay or an activity carries, for the same reason: a
    // leg bought by forwarding a confirmation has a code on it, and the
    // card said nothing about either.
    if (segment.booked) {
      route.append(node('span', 'item-booked', bookedMark(segment)))
    }
    const actions = node('div', 'segment-head-actions')
    actions.append(node('time', 'segment-date', dateLabel(segment.date)))
    const removeSlot = node('span', 'segment-remove')
    removeSlot.append(segmentRemoveButton(trip, segment, removeSlot))
    actions.append(removeSlot)
    head.append(route, actions)
    card.append(head)

    if (!segment.candidates.length) {
      card.append(node('p', 'no-options', noFlightLine(segment)))
      return card
    }
    const options = node('div', 'option-list')
    for (const candidate of segment.candidates) options.append(renderOption(trip, segment, candidate))
    card.append(options)
    return card
  }

  function renderTripDetail() {
    const trip = trips.find((item) => item.name === currentTrip)
    // Every caller of this function just changed which trip is selected —
    // a load, a click in the list, a saved choice — so this is the one
    // place that has to run whichever of those happened.
    updateComposeTarget()
    if (!trip) {
      showTripEmpty(
        trips.length ? 'Choose a trip' : 'No trips yet',
        trips.length
          ? 'Pick an itinerary to see its route and flights.'
          : 'Build a trip with Scout in chat, save flight options, then compare them here.',
      )
      return
    }

    tripDetail.replaceChildren()
    const head = node('div', 'trip-head')
    const title = node('div')
    title.append(
      node('p', 'eyebrow', tripRoute(trip)),
      node('h2', 'trip-title', trip.name),
      node('p', 'trip-subtitle', `${trip.adults} ${trip.adults === 1 ? 'traveller' : 'travellers'} · ${trip.cabin_class ?? 'Cabin not set'}`),
    )
    const actions = node('div', 'trip-head-actions')
    // Same warning as the list row, plus the one control that resolves it.
    // Omitted entirely once `trip.kept` — a kept trip has nothing left for
    // either of these to say or do.
    if (!trip.kept) {
      actions.append(node('span', 'status-chip draft', DRAFT_NOTE))
      const keep = node('button', 'trip-keep', 'Keep')
      keep.type = 'button'
      keep.addEventListener('click', () => keepTrip(trip, keep).catch(() => {}))
      actions.append(keep)
    }
    const download = node('button', 'trip-download', 'Download PDF')
    download.type = 'button'
    download.addEventListener('click', () => downloadTripPdf(trip, download))
    actions.append(node('span', `status-chip ${trip.status}`, trip.status), download)
    // Round and icon-only where `.trip-keep`/`.trip-download` are text
    // pills, and last in the row: a third pill reading "Delete" among them
    // is exactly the "destructive control beside a similar one" the
    // kept-trips spec declined this feature over, so the shape has to be
    // the thing that tells them apart, not just a colour. It opens
    // `deleteSlot` below rather than swapping itself out in place, the way
    // `segmentRemoveButton` does — the confirm needs room for a full
    // sentence, and a header pill's worth of space is not that.
    const deleteButton = node('button', 'trip-delete-button', '×')
    deleteButton.type = 'button'
    deleteButton.setAttribute('aria-label', `Delete ${trip.name}`)
    actions.append(deleteButton)
    head.append(title, actions)
    tripDetail.append(head)
    const deleteSlot = node('div', 'trip-delete-slot')
    tripDetail.append(deleteSlot)
    deleteButton.addEventListener('click', () => {
      deleteSlot.replaceChildren(tripDeleteConfirm(trip, deleteSlot))
    })
    if (trip.items.length) tripDetail.append(renderOverview(trip))

    const readiness = node('div', trip.not_ready ? 'trip-alert' : 'trip-alert ready')
    readiness.append(
      node('strong', '', trip.not_ready ? 'Needs a decision.' : 'Ready to price.'),
      document.createTextNode(` ${trip.not_ready ?? 'Every segment has a flight selected. Ask Scout in chat to refresh live fares and compare one ticket with separate bookings.'}`),
    )
    tripDetail.append(readiness)
    for (const note of trip.notes ?? []) {
      const alert = node('div', 'trip-alert')
      alert.append(node('strong', '', 'Connection check.'), document.createTextNode(` ${note}`))
      tripDetail.append(alert)
    }

    // Items and the mail waiting to become one, in one timeline — see
    // `pendingRowsFor`. Drawn even when the trip has no items yet: a draft
    // an arrival started holds nothing but its pending row, and that row
    // is the whole reason the trip is on the list.
    const stack = node('div', 'segment-stack')
    // Counts item rows as they pass: the join below looks past this item
    // in `trip.items`, and pending rows are not in that list.
    let itemIndex = 0
    for (const row of pendingRowsFor(trip, inbox?.pending ?? [])) {
      if (row.kind === 'pending') {
        stack.append(renderPendingRow(trip, row.arrival))
        continue
      }
      const item = row.item
      itemIndex++
      stack.append(renderItem(trip, item))
      // The join is checked from one flight to the next flight, whatever
      // sits between them: a stay does not change when the second leg
      // leaves. The PDF draws it in the same place, under the first flight.
      if (item.kind !== 'flight') continue
      const next = trip.items.slice(itemIndex).find((later) => later.kind === 'flight')
      if (next) {
        const check = connectionCheck(item, next)
        stack.append(node('div', `join-card ${check.tone}`, check.text))
      }
    }
    tripDetail.append(stack)
    tripDetail.append(renderAddLegForm(trip))
  }

  // An arrival on the timeline: the card its booking would become, drawn
  // dashed, with the three things the reader can say about it. Nothing on
  // it is editable — a wrong guess is "Not this trip" or "Ignore" — and
  // what it shows is what the extractor read from the mail, put in front
  // of the reader so they can check it before it becomes an item. The `?`
  // sits where an item's Remove button does: this card has no place on
  // the trip yet, and that is the one thing the head has to say.
  function renderPendingRow(trip, arrival) {
    const card = node('article', 'item-card pending')
    const head = node('header', 'segment-head')
    const about = node('div')
    const flight = arrival.kind === 'flight' && arrival.origin && arrival.destination
    about.append(
      node('p', 'segment-kicker', `Arrived · ${arrival.kind ? kindLabel(arrival) : 'Booking'}`),
      node('h3', 'item-title', flight ? `${arrival.origin} → ${arrival.destination}` : arrival.title || arrival.summary || 'Booking'),
    )
    if (arrival.place) about.append(node('p', 'item-place', arrival.place))
    const facts = []
    if (arrival.confirmation_code) facts.push(`confirmation ${arrival.confirmation_code}`)
    if (Number.isFinite(arrival.price)) facts.push(moneyLabel(arrival.price, arrival.currency))
    if (facts.length) about.append(node('p', 'pending-facts', facts.join(' · ')))
    if (arrival.attachments?.length) about.append(attachmentLinks(arrival.attachments))
    const actions = node('div', 'segment-head-actions')
    actions.append(node('time', 'segment-date', itemDateLabel(arrival)))
    const mark = node('span', 'pending-mark', '?')
    mark.setAttribute('title', 'Not on the trip yet')
    mark.setAttribute('aria-label', 'Not on the trip yet')
    actions.append(mark)
    head.append(about, actions)
    card.append(head)

    const row = node('div', 'pending-actions')
    // Adding to a draft keeps it — the store does that in one write — and
    // the label says so, because "Add" alone would put a booking into a
    // trip that then reads "Clears with its chat unless kept".
    const add = node('button', 'pending-add', trip.kept ? 'Add' : 'Keep this trip and add')
    add.type = 'button'
    add.addEventListener('click', () => {
      decideArrival(arrival, 'add', {}, (plan) => `Added to ${plan.name}.`).catch(() => {})
    })
    const elsewhere = node('button', '', 'Not this trip')
    elsewhere.type = 'button'
    const ignore = node('button', '', 'Ignore')
    ignore.type = 'button'
    ignore.addEventListener('click', () => {
      decideArrival(arrival, 'ignore', {}, () => 'Ignored. It is under Other mail.').catch(() => {})
    })
    row.append(add, elsewhere, ignore)
    card.append(row)
    // The picker opens under the buttons rather than replacing them, so
    // Ignore stays reachable while it is open; a second press, or Escape
    // anywhere on the card while it is open, closes it and puts focus
    // back on the button. On the card, not the slot: right after the
    // click that opened it, focus is still on the button.
    const pickerSlot = node('div', 'pending-picker-slot')
    pickerSlot.id = `arrival-${arrival.id}-picker`
    card.append(pickerSlot)
    function setPicker(open) {
      elsewhere.setAttribute('aria-expanded', String(open))
      pickerSlot.replaceChildren(...(open ? [tripPicker(trip, arrival)] : []))
    }
    elsewhere.setAttribute('aria-expanded', 'false')
    elsewhere.setAttribute('aria-controls', pickerSlot.id)
    elsewhere.addEventListener('click', () => setPicker(!pickerSlot.firstChild))
    card.addEventListener('keydown', (e) => {
      if (e.key !== 'Escape' || !pickerSlot.firstChild) return
      e.stopPropagation()
      setPicker(false)
      elsewhere.focus()
    })
    return card
  }

  // The other trips by name, then a new one: the body the server takes is
  // `{"trip": name}` or `{"new": true}`, and a Plan has no id to send
  // instead. The trip the card is on is left out — that is what Add is.
  function tripPicker(trip, arrival) {
    const picker = node('div', 'pending-picker')
    picker.setAttribute('role', 'group')
    picker.setAttribute('aria-label', 'Which trip is this for?')
    picker.append(node('span', 'pending-picker-label', 'Add it to'))
    for (const other of trips) {
      if (other.name === trip.name) continue
      const button = node('button', '', other.name)
      button.type = 'button'
      button.addEventListener('click', () => {
        decideArrival(arrival, 'add', { trip: other.name }, (plan) => `Added to ${plan.name}.`).catch(() => {})
      })
      picker.append(button)
    }
    const fresh = node('button', '', 'New trip')
    fresh.type = 'button'
    fresh.addEventListener('click', () => {
      decideArrival(arrival, 'add', { new: true }, (plan) => `Started ${plan.name} with it.`).catch(() => {})
    })
    picker.append(fresh)
    return picker
  }

  // Add or Ignore, the shape `addLeg` has: guard against an overlapping
  // write, invalidate the load sequence before the request goes out. Both
  // trips and inbox are then reloaded rather than patched from the
  // response — an add changes one trip and removes one pending row, and
  // `loadTrips` fetches both in one go. A 409 is not an error: the phone
  // or another tab decided this one first, and the reload shows what
  // stands. `done(result)` words the toast; the result is the Plan for an
  // add and `{}` for an ignore.
  async function decideArrival(arrival, verb, body, done) {
    if (tripChoicePending) return
    tripChoicePending = true
    tripLoadSeq++
    tripDetail.setAttribute('aria-busy', 'true')
    // Every button on this card, picker included, is off until the answer:
    // `tripChoicePending` already drops a second press on the floor, but
    // a button that still looks live invites it. Any reload repaints the
    // card; the `finally` is for the refusals that leave it standing.
    const card = document.getElementById(`arrival-${arrival.id}-picker`)?.closest('.item-card')
    const buttons = [...(card?.querySelectorAll('.pending-actions button, .pending-picker button') ?? [])]
    for (const button of buttons) button.disabled = true
    try {
      const res = await post(`/chat/arrivals/${encodeURIComponent(arrival.id)}/${verb}`, body)
      if (res.status === 409) {
        tripChoicePending = false
        tripsLoaded = false
        await loadTrips()
        return
      }
      if (!res.ok) {
        const reason = await refusalReason(res)
        showTripToast(reason ? `Could not do that: ${reason}.` : 'Could not do that. Try again.')
        return
      }
      const result = await res.json()
      // An add answers with the trip it landed in. Selecting it is how a
      // "Not this trip" or "New trip" shows where the booking went rather
      // than leaving the reader on the card that just vanished.
      if (typeof result?.name === 'string') currentTrip = result.name
      tripChoicePending = false
      tripsLoaded = false
      await loadTrips()
      showTripToast(done(result))
    } catch {
      showTripToast('Could not reach Scout. Try again.')
    } finally {
      tripChoicePending = false
      tripDetail.removeAttribute('aria-busy')
      for (const button of buttons) button.disabled = false
    }
  }

  // Every refusal from the inbox routes is `{"ok":false,"reason"}` under
  // its status; this reads the reason out and gives up quietly on a body
  // that is anything else.
  async function refusalReason(res) {
    try {
      const body = await res.json()
      return typeof body?.reason === 'string' && body.reason ? body.reason : null
    } catch {
      return null
    }
  }

  // Plain anchors: the route sets `Content-Disposition`, and a click on
  // one is a download with nothing for the page to do.
  function attachmentLinks(attachments) {
    const list = node('p', 'attachment-links')
    for (const file of attachments) {
      const link = node('a', '', file.filename || `attachment ${file.id}`)
      link.href = `/chat/attachments/${encodeURIComponent(file.id)}`
      link.setAttribute('download', '')
      list.append(link)
    }
    return list
  }

  // The mail that did not become a pending row, under the trip list: one
  // line each, see `otherMailLines`. Hidden outright when there is none —
  // an empty "Other mail" heading is a question the reader did not ask.
  // "Add by hand" for a mail Scout could not read points at chat, where
  // the item can be described; a form for it is not in this slice.
  function renderOtherMail() {
    if (!otherMailEl) return
    const lines = otherMailLines(inbox?.other ?? [])
    otherMailEl.replaceChildren()
    otherMailEl.hidden = !lines.length
    if (!lines.length) return
    const title = node('h2', 'other-mail-title', 'Other mail')
    title.id = 'other-mail-title'
    otherMailEl.append(title)
    const list = node('ul', 'other-mail-list')
    for (const line of lines) {
      const li = node('li', 'other-mail-row')
      const top = node('div', 'other-mail-top')
      const who = node('span', 'other-mail-sender', line.sender)
      // The bare address beside the name, always — see `otherMailLines`.
      // Omitted only when it is the name, which would print it twice.
      if (line.address !== line.sender) who.append(node('span', 'other-mail-address', line.address))
      // The date and the × travel together at the right, so the confirm
      // that replaces the × takes the date's line with it when the row is
      // too narrow for both, rather than splitting the sender.
      const end = node('div', 'other-mail-end')
      const removeSlot = node('span', 'other-mail-remove')
      removeSlot.append(mailRemoveButton(line, removeSlot))
      end.append(node('span', 'other-mail-when', line.when), removeSlot)
      top.append(who, end)
      li.append(top, node('p', 'other-mail-subject', line.subject))
      li.append(node('p', 'other-mail-meta', `${line.reason} · ${line.note}`))
      if (line.attachments.length) li.append(attachmentLinks(line.attachments))
      if (line.failed) {
        const byHand = node('button', 'other-mail-add', 'Add by hand')
        byHand.type = 'button'
        byHand.addEventListener('click', () => {
          showTripToast("Ask Scout in chat: 'I've booked …'")
        })
        li.append(byHand)
      }
      list.append(li)
    }
    otherMailEl.append(list)
  }

  // Whether the row's right-hand group is showing more than the date and
  // the ×. It is allowed to wrap onto its own line only then: in the
  // resting state a wrap puts a stray date and × on a line of their own,
  // and at the aside's width flex breaks on max-content, so the stacked
  // sender block triggers it on ordinary rows.
  function mailRowWide(slot, wide) {
    slot.closest('.other-mail-row')?.classList.toggle('confirming', wide)
  }

  // The × a row starts with. Its own function, as `segmentRemoveButton` is,
  // so the confirm's Cancel and a refusal can both rebuild exactly this.
  function mailRemoveButton(line, slot) {
    const button = node('button', 'segment-remove-button other-mail-remove-button', '×')
    button.type = 'button'
    button.setAttribute('aria-label', otherMailDeleteLabel(line))
    button.addEventListener('click', () => {
      const row = mailRemoveConfirm(line, slot)
      mailRowWide(slot, true)
      slot.replaceChildren(row)
      // Focus was on the × this just replaced. Cancel is first in the row
      // and is the safe half of the choice, so it takes it — and Delete is
      // still dead for the moment below, so focus could not go there.
      row.querySelector('button')?.focus()
    })
    return button
  }

  // A second press rather than `window.confirm`, for the reason
  // `removeConfirmRow` gives — and the same swap-in-place: the ask stands
  // where the × was and puts the × back on Cancel or on a failure. Only a
  // delete that happened leaves it gone, and that repaints the whole list
  // anyway.
  function mailRemoveConfirm(line, slot) {
    const row = node('span', 'segment-remove-confirm')
    row.append(node('span', '', 'Delete?'))
    const cancel = node('button', '', 'Cancel')
    cancel.type = 'button'
    cancel.addEventListener('click', () => {
      mailRowWide(slot, false)
      slot.replaceChildren(mailRemoveButton(line, slot))
    })
    const confirm = node('button', 'danger', 'Delete')
    confirm.type = 'button'
    armConfirm(confirm)
    confirm.addEventListener('click', () => {
      // Off until the answer, the way `decideArrival` disables the card it
      // was pressed on: a button that still looks live invites a second
      // press at a mail that is already going.
      confirm.disabled = true
      cancel.disabled = true
      deleteMail(line).then((problem) => {
        if (problem) mailRemoveProblem(line, slot, problem)
      }).catch(() => {
        mailRemoveProblem(line, slot, 'Could not reach Scout.')
      })
    })
    row.append(cancel, confirm)
    return row
  }

  // A refusal belongs in the row it was pressed in. `showTripToast` appends
  // into the right-hand pane, and on a phone Other mail sits below the
  // trip, so the sentence explaining the refusal would land off-screen —
  // the reader would see a × that did nothing. The × comes back beside the
  // reason, because both refusals are answered by acting and pressing
  // again: decide the booking, or let the read finish.
  function mailRemoveProblem(line, slot, text) {
    const row = node('span', 'other-mail-problem')
    const said = node('span', '', text)
    said.id = `mail-${line.mail_id}-problem`
    const button = mailRemoveButton(line, slot)
    // Described by the reason rather than announced by a live region: this
    // node is inserted with its text already in it, which a live region is
    // not obliged to read out, and focus lands on the button in the same
    // breath — so the description is what a reader actually hears.
    button.setAttribute('aria-describedby', said.id)
    row.append(said, button)
    mailRowWide(slot, true)
    slot.replaceChildren(row)
    button.focus()
  }

  // Focus was on the × of a row that no longer exists, and a repaint that
  // leaves it on nothing drops it to the body — which sends a keyboard
  // reader back to the top of the page, the hazard `closeDrawer` names. It
  // goes to the × that took this row's place, to the last × when the row
  // that went was last, and to the address line below the section when
  // there are no rows left and the section hides itself.
  function focusAfterMailDelete(index) {
    const buttons = [...(otherMailEl?.querySelectorAll('.other-mail-remove-button') ?? [])]
    const next = buttons[Math.min(index, buttons.length - 1)]
    ;(next ?? handleLineEl?.querySelector('button'))?.focus()
  }

  // The reason to put back in the row, or `null` when the row is gone and
  // the list has been repainted. A 404 is the second of those: the mail is
  // already gone, all but always because another tab deleted it, and
  // restoring the × would leave a row with nothing behind it.
  //
  // Takes the same three lines every other write on this page takes — the
  // guard, the flag, the sequence bump — because a `loadTrips` already in
  // flight would otherwise repaint the deleted row back with a live ×.
  async function deleteMail(line) {
    if (tripChoicePending) return 'Something else is saving.'
    tripChoicePending = true
    tripLoadSeq++
    const index = Math.max(0, (inbox?.other ?? []).findIndex((row) => row.mail_id === line.mail_id))
    try {
      const res = await fetch(`/chat/mail/${encodeURIComponent(line.mail_id)}`, {
        method: 'DELETE',
        headers: { 'x-scout-csrf': csrfToken },
      })
      if (!res.ok && res.status !== 404) {
        const reason = await refusalReason(res)
        return reason ? `${reason[0].toUpperCase()}${reason.slice(1)}.` : 'Could not delete it.'
      }
      // The row is gone whatever the reload then says, so it leaves this
      // tab's copy first: an inbox that fails to load must not leave a
      // deleted mail on screen with a live × on it.
      inbox = { ...inbox, other: (inbox?.other ?? []).filter((row) => row.mail_id !== line.mail_id) }
      const loaded = await fetchInbox()
      if (loaded) inbox = loaded
      renderInboxSide()
      // Nothing is said on success: the row leaving is the whole answer.
      focusAfterMailDelete(index)
      return null
    } catch {
      return 'Could not reach Scout.'
    } finally {
      tripChoicePending = false
    }
  }

  // The address line at the foot of the aside. Three states: nothing
  // (the inbox has not answered, or never will), the form (no handle yet,
  // or Change pressed), and the address with Copy and Change.
  //
  // Called on every reload — a tab waking, every arrival verb — and a
  // form the reader is in the middle of is left alone then: a rebuild
  // would wipe the half-typed handle and the hint under it. "In the
  // middle of" is the input focused or holding something other than the
  // saved handle; Save and Cancel go through `renderHandle` with the
  // form's own state settled, so those still repaint.
  function renderHandle() {
    if (!handleLineEl) return
    const input = handleLineEl.querySelector('.handle-form input')
    if (input && inbox && (document.activeElement === input || input.value !== (inbox.handle ?? ''))) return
    handleLineEl.replaceChildren()
    handleLineEl.hidden = !inbox
    if (!inbox) return
    if (!inbox.handle || handleEditing) {
      handleLineEl.append(handleForm())
      return
    }
    const address = `${inbox.handle}@${inbox.domain}`
    handleLineEl.append(node('p', 'handle-label', 'Your booking address'))
    handleLineEl.append(node('p', 'handle-address', address))
    const row = node('div', 'handle-row')
    const copy = node('button', '', 'Copy')
    copy.type = 'button'
    copy.addEventListener('click', () => {
      navigator.clipboard.writeText(address)
        .then(() => showTripToast('Copied'))
        .catch(() => showTripToast('Could not copy. Select the address and copy it by hand.'))
    })
    const change = node('button', '', 'Change')
    change.type = 'button'
    change.addEventListener('click', () => {
      handleEditing = true
      renderHandle()
    })
    row.append(copy, change)
    handleLineEl.append(row)
  }

  // `handleProblem` speaks on every keystroke — the rules are the
  // server's, mirrored — and only a handle that passes them is sent to
  // `/chat/handle/check`, 400 ms after the last key. A 429 from the check
  // says nothing: the reader is still typing, and "slow down" under an
  // input is a complaint about the page, not the handle. Save posts
  // whatever is typed and shows the server's reason on a 409 or 422.
  function handleForm() {
    const form = document.createElement('form')
    form.className = 'handle-form'
    form.append(node('p', 'handle-label', inbox.handle
      ? 'Change your booking address'
      : 'Forward bookings to an address of your own'))
    const row = node('div', 'handle-form-row')
    const input = document.createElement('input')
    input.type = 'text'
    // No `maxLength`: a pasted 31-character handle should get the "3 to
    // 30" sentence, not a silent cut to something the reader did not type.
    input.autocomplete = 'off'
    input.spellcheck = false
    input.placeholder = 'yourname'
    input.value = inbox.handle ?? ''
    input.setAttribute('aria-label', 'Handle')
    input.setAttribute('aria-describedby', 'handle-hint')
    const save = node('button', 'handle-save', 'Save')
    save.type = 'submit'
    row.append(input, node('span', 'handle-suffix', `@${inbox.domain}`), save)
    const hint = node('p', 'handle-hint')
    hint.id = 'handle-hint'
    form.append(row, hint)
    function say(text, problem) {
      hint.className = problem ? 'handle-hint problem' : 'handle-hint'
      hint.textContent = text
    }

    // A check that answers after the input moved on is about a handle
    // nobody is looking at any more; the timer is the one not yet sent.
    let checkSeq = 0
    let checkTimer = null

    if (inbox.handle) {
      const cancel = node('button', 'handle-cancel', 'Cancel')
      cancel.type = 'button'
      cancel.addEventListener('click', () => {
        clearTimeout(checkTimer)
        checkSeq++
        handleEditing = false
        // Back to the saved handle first, or `renderHandle` reads the
        // abandoned edit as one still in progress and keeps the form.
        input.value = inbox.handle
        renderHandle()
      })
      row.append(cancel)
    }
    async function checkHandle(raw, seq) {
      try {
        const res = await fetch(`/chat/handle/check?handle=${encodeURIComponent(raw)}`)
        if (seq !== checkSeq || !res.ok) return
        const answer = await res.json()
        if (seq !== checkSeq) return
        if (answer.ok) say(`${answer.handle}@${inbox.domain} is free`, false)
        else if (typeof answer.reason === 'string') say(answer.reason, true)
      } catch {
        // Silence, as for a 429: the Save will say what the check could not.
      }
    }

    input.addEventListener('input', () => {
      clearTimeout(checkTimer)
      checkSeq++
      const raw = input.value
      if (!raw.trim()) return say('', false)
      const problem = handleProblem(raw)
      say(problem ?? '', Boolean(problem))
      if (problem) return
      const seq = checkSeq
      checkTimer = setTimeout(() => checkHandle(raw, seq).catch(() => {}), 400)
    })

    form.addEventListener('submit', (event) => {
      event.preventDefault()
      const raw = input.value
      const problem = handleProblem(raw)
      if (problem) {
        say(problem, true)
        input.focus()
        return
      }
      clearTimeout(checkTimer)
      checkSeq++
      save.disabled = true
      saveHandle(raw).catch(() => {}).finally(() => {
        save.disabled = false
      })
    })

    async function saveHandle(raw) {
      try {
        const res = await post('/chat/handle', { handle: raw })
        if (!res.ok) {
          say(await refusalReason(res) ?? 'Could not save that. Try again.', true)
          return
        }
        const saved = await res.json()
        handleEditing = false
        inbox = { ...inbox, handle: saved.handle }
        // The form's own state is settled: value and focus both cleared
        // so `renderHandle` does not read a finished edit as one in flight.
        input.value = saved.handle
        input.blur()
        renderHandle()
        showTripToast(`Your booking address is ${saved.handle}@${inbox.domain}.`)
      } catch {
        say('Could not reach Scout. Try again.', true)
      }
    }
    return form
  }

  // A second press, not `window.confirm`, for the reason `removeConfirmRow`
  // gives — but built as its own block below the header rather than in
  // place of the × that opened it: `tripDeleteConsequence` can run to a full
  // sentence ("Its 2 legs and 5 saved flight options go with it."), and that
  // needs a paragraph, not the width of one header pill. Styled like
  // `.trip-alert` with the border `.join-card.danger` uses, so it reads as a
  // warning rather than routine trip info sitting under the title.
  function tripDeleteConfirm(trip, slot) {
    const box = node('div', 'trip-delete-confirm')
    box.append(node('p', '', `Delete this trip? ${tripDeleteConsequence(trip)}`))
    const row = node('div', 'trip-delete-confirm-row')
    const cancel = document.createElement('button')
    cancel.type = 'button'
    cancel.textContent = 'Cancel'
    cancel.addEventListener('click', () => slot.replaceChildren())
    const confirm = document.createElement('button')
    confirm.type = 'button'
    confirm.className = 'danger'
    confirm.textContent = 'Delete trip'
    confirm.addEventListener('click', () => {
      confirm.disabled = true
      cancel.disabled = true
      deleteTrip(trip).then((restore) => {
        if (restore) slot.replaceChildren(tripDeleteConfirm(trip, slot))
      }).catch(() => {
        slot.replaceChildren(tripDeleteConfirm(trip, slot))
      })
    })
    row.append(cancel, confirm)
    box.append(row)
    return box
  }

  // No position: the store puts the leg where its departure date belongs.
  // The markup for choosing a place in the itinerary does not exist, and
  // does not need to — a dated leg has exactly one place it goes, and
  // sending a position would be this form overriding that with a guess.
  function renderAddLegForm(trip) {
    const form = document.createElement('form')
    form.className = 'leg-add'
    form.append(node('p', 'leg-add-title', 'Add a leg'))
    const row = node('div', 'leg-add-row')

    function field(labelText, type, placeholder) {
      const label = node('label', '', labelText)
      const input = document.createElement('input')
      input.type = type
      input.required = true
      if (placeholder) input.placeholder = placeholder
      if (type === 'text') {
        input.maxLength = 3
        input.autocomplete = 'off'
        // The wire format is uppercase IATA; showing it uppercase as it's
        // typed means what is submitted is what the reader sees, not a
        // silent rewrite. `.leg-add-code` carries the rule in the
        // stylesheet rather than setting it here per element.
        input.classList.add('leg-add-code')
      }
      label.append(input)
      row.append(label)
      return input
    }

    const origin = field('From', 'text', 'AMS')
    const destination = field('To', 'text', 'FCO')
    const date = field('Depart', 'date')
    const submit = document.createElement('button')
    submit.type = 'submit'
    submit.textContent = 'Add leg'
    row.append(submit)
    form.append(row)

    form.addEventListener('submit', (e) => {
      e.preventDefault()
      if (submit.disabled) return
      submit.disabled = true
      addLeg(trip, origin.value.trim().toUpperCase(), destination.value.trim().toUpperCase(), date.value)
        .finally(() => { submit.disabled = false })
    })
    return form
  }

  async function chooseFlight(trip, segment, candidate, optionEl) {
    // Every response contains a whole-trip snapshot. Serialize choices so
    // responses for two rapid clicks cannot repaint one another out of order.
    if (tripChoicePending) return
    tripChoicePending = true
    // Invalidates any GET that started before this write.
    tripLoadSeq++
    tripDetail.setAttribute('aria-busy', 'true')
    optionEl.classList.add('saving')
    try {
      const res = await post('/chat/trips/choice', {
        trip: trip.name,
        position: segment.position,
        candidate: candidate.candidate,
      })
      if (res.status === 404) {
        tripChoicePending = false
        tripsLoaded = false
        await loadTrips()
        showTripToast('That option changed. The trip has been refreshed.')
        return
      }
      if (!res.ok) throw new Error('refused')
      const updated = await res.json()
      trips = [updated, ...trips.filter((item) => item.name !== updated.name)]
      currentTrip = updated.name
      renderTripList()
      renderTripDetail()
      showTripToast(`Option ${candidate.candidate} selected for ${segment.origin} → ${segment.destination}.`)
      tripDetail
        .querySelector(`input[name="segment-${segment.position}"][data-candidate="${candidate.candidate}"]`)
        ?.focus()
    } catch {
      optionEl.classList.remove('saving')
      showTripToast('Could not save that choice. Try again.')
    } finally {
      tripChoicePending = false
      tripDetail.removeAttribute('aria-busy')
    }
  }

  // The one-press answer to "Save this trip" — see `KeepIn` in
  // `routes/trips.rs` for why this exists as its own route rather than a
  // chat turn: a model call is slower than a press, and asking one to
  // interpret "save this trip" is how it became `record_purchase` in
  // production instead. Same shape as `chooseFlight`: guarded against an
  // overlapping write, the load sequence invalidated before the request
  // goes out, and the response's whole-trip snapshot — never a local flip
  // of `kept` — is what redraws the page.
  async function keepTrip(trip, button) {
    if (tripChoicePending) return
    tripChoicePending = true
    tripLoadSeq++
    tripDetail.setAttribute('aria-busy', 'true')
    button.disabled = true
    try {
      const res = await fetch('/chat/trips/keep', {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'x-scout-csrf': csrfToken },
        body: keepBody(trip.name),
      })
      if (res.status === 404) {
        tripChoicePending = false
        tripsLoaded = false
        await loadTrips()
        showTripToast('That trip is gone. The list has been refreshed.')
        return
      }
      if (!res.ok) throw new Error('refused')
      const updated = await res.json()
      trips = [updated, ...trips.filter((item) => item.name !== updated.name)]
      currentTrip = updated.name
      renderTripList()
      renderTripDetail()
      showTripToast(`${updated.name} is kept — it will not clear with its chat.`)
    } catch {
      button.disabled = false
      showTripToast('Could not keep that trip. Try again.')
    } finally {
      tripChoicePending = false
      tripDetail.removeAttribute('aria-busy')
    }
  }

  // Returns whether `tripDeleteConfirm` should be rebuilt in the slot it
  // came from — the same convention `removeItem` uses, and true for the same
  // reason: only a response that actually rewrote the account's trips (200,
  // or the reload a 404 triggers) is allowed to leave the confirm gone for
  // good. `trip` here is always `currentTrip` — the × that opens the
  // confirm lives in this trip's own header, not the list — so the response
  // naming what is left can never describe a trip other than the one just
  // shown, which is what decides where the selection lands below.
  async function deleteTrip(trip) {
    if (tripChoicePending) return true
    tripChoicePending = true
    tripLoadSeq++
    tripDetail.setAttribute('aria-busy', 'true')
    try {
      const res = await fetch('/chat/trips', {
        method: 'DELETE',
        headers: { 'content-type': 'application/json', 'x-scout-csrf': csrfToken },
        body: deleteTripBody(trip.name),
      })
      if (res.status === 404) {
        // Already gone — a second tab, or a second press racing the first.
        // Same move `keepTrip` makes for the same status: there is nothing
        // left to argue with the server about, so ask it what remains.
        tripChoicePending = false
        tripsLoaded = false
        await loadTrips()
        showTripToast('That trip is already gone.')
        return false
      }
      if (!res.ok) throw new Error('refused')
      trips = await res.json()
      tripCount.textContent = String(trips.length)
      tripCount.hidden = trips.length === 0
      // The deleted trip cannot still be in `trips` — see above — so this
      // is the same fallback `loadTrips` uses for a selection that isn't
      // there any more: first trip in the list, or the empty state
      // `renderTripDetail` already draws when `trips` is empty.
      currentTrip = trips[0]?.name ?? null
      renderTripList()
      renderTripDetail()
      showTripToast(`${trip.name} deleted.`)
      return false
    } catch {
      showTripToast('Could not delete that trip. Try again.')
      return true
    } finally {
      tripChoicePending = false
      tripDetail.removeAttribute('aria-busy')
    }
  }

  async function downloadTripPdf(trip, button) {
    if (button.disabled) return
    const label = button.textContent
    button.disabled = true
    button.setAttribute('aria-busy', 'true')
    button.textContent = 'Preparing…'
    try {
      const res = await post('/chat/trips/pdf', { trip: trip.name })
      if (!res.ok) throw new Error('PDF request failed')
      const url = URL.createObjectURL(await res.blob())
      const link = document.createElement('a')
      link.href = url
      link.download = tripPdfFilename(trip.name)
      document.body.append(link)
      link.click()
      link.remove()
      setTimeout(() => URL.revokeObjectURL(url), 1000)
      showTripToast('Trip PDF downloaded.')
    } catch {
      showTripToast('Could not create the PDF. Try again in a moment.')
    } finally {
      button.disabled = false
      button.removeAttribute('aria-busy')
      button.textContent = label
      button.focus()
    }
  }

  function showTripToast(text) {
    tripDetail.querySelector('.trip-toast')?.remove()
    const toast = node('div', 'trip-toast', text)
    toast.setAttribute('role', 'status')
    tripDetail.append(toast)
  }

  // Adds a leg; the server puts it where its date falls, so the body names
  // no position. Same shape as `chooseFlight`: guard against an overlapping
  // write, invalidate the load sequence before the request goes out, and
  // repaint from the response's whole-trip snapshot.
  async function addLeg(trip, origin, destination, departureDate) {
    if (tripChoicePending) return
    tripChoicePending = true
    tripLoadSeq++
    tripDetail.setAttribute('aria-busy', 'true')
    try {
      const res = await post('/chat/trips/segment', {
        trip: trip.name, origin, destination, departure_date: departureDate,
      })
      if (res.status === 409) {
        // Not an error: this tab's copy is simply older than the trip, and
        // the itinerary a leg was being placed into has moved since it was
        // drawn — so the honest move is to reload rather than retry blind.
        tripChoicePending = false
        tripsLoaded = false
        await loadTrips()
        showTripToast('This trip changed elsewhere. Showing the current itinerary.')
        return
      }
      if (res.status === 422) {
        // The body is the message to show — it names what to fix (a bad
        // airport code, a bad date, a route with the same place twice).
        showTripToast(await res.text())
        return
      }
      if (!res.ok) throw new Error('refused')
      const updated = await res.json()
      trips = [updated, ...trips.filter((item) => item.name !== updated.name)]
      currentTrip = updated.name
      renderTripList()
      renderTripDetail()
      showTripToast(`Added ${origin} → ${destination}.`)
    } catch {
      showTripToast('Could not add that leg. Try again.')
    } finally {
      tripChoicePending = false
      tripDetail.removeAttribute('aria-busy')
    }
  }

  // Returns whether the confirm row that called this should revert to the
  // plain Remove button. That is only true when nothing redrew the trip —
  // a 422, or the request never landing. On 200 and on the reload a 409
  // triggers, `renderTripDetail` already rebuilt this card from scratch,
  // so the caller has nothing left to put back.
  async function removeItem(trip, item) {
    if (tripChoicePending) return false
    tripChoicePending = true
    tripLoadSeq++
    tripDetail.setAttribute('aria-busy', 'true')
    try {
      const res = await fetch('/chat/trips/segment', {
        method: 'DELETE',
        headers: { 'content-type': 'application/json', 'x-scout-csrf': csrfToken },
        body: removeItemBody(trip.name, item),
      })
      if (res.status === 409) {
        // Not an error: this tab's copy is simply older than the trip — the
        // renumbering a remove does server-side means the position this
        // click named may no longer be the item it was clicked on.
        tripChoicePending = false
        tripsLoaded = false
        await loadTrips()
        showTripToast('This trip changed elsewhere. Showing the current itinerary.')
        return false
      }
      if (res.status === 422) {
        showTripToast(await res.text())
        return true
      }
      if (!res.ok) throw new Error('refused')
      const updated = await res.json()
      trips = [updated, ...trips.filter((item) => item.name !== updated.name)]
      currentTrip = updated.name
      renderTripList()
      renderTripDetail()
      showTripToast(`Removed ${itemName(item)}.`)
      return false
    } catch {
      showTripToast(`Could not remove that ${itemNoun(item)}. Try again.`)
      return true
    } finally {
      tripChoicePending = false
      tripDetail.removeAttribute('aria-busy')
    }
  }

  async function loadHistory() {
    const res = await fetch('/chat/history')
    if (!res.ok) {
      showNotice('Could not load the conversation so far. Reload to try again.')
      return
    }
    showTurns(await res.json())
    // The transcript just drawn is the server's current thread, so this is
    // one of the two refreshes that may adopt the server's answer.
    await refreshThreads(true)
  }

  // Replaces the transcript wholesale rather than appending: every caller
  // is switching what the page is showing, not adding to it.
  function showTurns(turns) {
    turnsEl.replaceChildren()
    for (const turn of turns) turnsEl.append(turnElement(turn.role, turn.text, turn.run_id))
    turnsEl.scrollTop = turnsEl.scrollHeight
  }

  // Every thread route is a POST behind the CSRF header, and most carry no
  // body at all — so the header set lives here once rather than at each of
  // the eight call sites.
  async function post(path, body) {
    return fetch(path, {
      method: 'POST',
      headers: { 'content-type': 'application/json', 'x-scout-csrf': csrfToken },
      body: body === undefined ? undefined : JSON.stringify(body),
    })
  }

  // Redraws the list. What the composer sends into is decided by
  // `resolveCurrent`, not taken from the server's `current` flag — the
  // reader's transcript is the thing that names the thread.
  async function refreshThreads(adopt = false) {
    const seq = ++refreshSeq
    let list
    try {
      const res = await fetch('/chat/threads')
      if (!res.ok) return
      list = await res.json()
    } catch {
      // A tab waking up on a dead network. The list on screen is stale
      // rather than wrong, and a console error helps nobody.
      return
    }
    // A refresh started after this one has already answered: its list is
    // the newer truth, and painting this one over it would undo it.
    if (seq !== refreshSeq) return
    // The thread on screen is no longer in the list. Handing the composer to
    // the server's current thread and stopping there would leave the old
    // transcript up with nothing said, and the next message would go to a
    // conversation the reader never opened. `vanished` says it, adopts, and
    // redraws the transcript — and its own refresh passes `adopt`, so this
    // branch cannot be taken a second time.
    if (threadVanished(list, currentThread, adopt)) {
      renderThreads(list)
      await vanished()
      return
    }
    currentThread = resolveCurrent(list, currentThread, adopt)
    renderThreads(list)
  }

  // The highlight follows `currentThread` — what this page is showing —
  // and not `thread.current`, which is the server's separate answer.
  function renderThreads(list) {
    lastList = list
    threadsEl.replaceChildren()
    for (const thread of list) threadsEl.append(threadRow(thread, thread.id === currentThread))
  }

  // How often the "2h" / "expires in 12h" labels are recomputed. The
  // coarsest thing they say changes by is an hour, so a minute is already
  // far finer than it needs to be — and cheap enough to be the safe choice
  // for the row that ticks from "expires in 1h" to gone.
  const WHEN_TICK_MS = 60_000

  // Re-labels the rows already on screen from the list they were built
  // from. `whenLabel` runs at render time, so a tab left open all afternoon
  // kept saying "expires in 12h" about a thread with two hours left — the
  // one number on this page whose whole purpose is to be watched.
  //
  // Deliberately narrow: not a refetch, because nothing has changed on the
  // server that this needs to learn — only the clock moved — and a request
  // a minute from every idle tab is a poll nobody asked for. And not a
  // re-render either: `renderThreads` replaces every row, which would throw
  // away a rename input the reader is halfway through typing into. Only the
  // `.when` span of each row is touched, and a row with none (that rename
  // in progress) is left alone.
  function tickWhenLabels() {
    // A hidden tab's labels are seen by nobody, and `visibilitychange`
    // refreshes the list outright when it comes back.
    if (document.hidden) return
    const rows = threadsEl.children
    if (rows.length !== lastList.length) return
    let anyExpired = false
    for (let i = 0; i < lastList.length; i++) {
      const whenEl = rows[i].querySelector('.when')
      if (!whenEl) continue
      const thread = lastList[i]
      const when = whenLabel(thread)
      whenEl.textContent = when.text
      whenEl.className = when.expiring ? 'when expiring' : 'when'
      // `whenLabel`'s countdown pins at "expires in 1h" once the window is
      // under an hour — `Math.max(1, ...)` never goes lower — so a tab left
      // open past the real deadline would keep reading a thread as an hour
      // from gone, forever. The thread is deleted server-side the moment
      // its age actually reaches 48h; catching that here and refreshing
      // once is what drops the row instead of leaving a countdown that has
      // stopped meaning anything.
      if (!anyExpired && when.text.startsWith('expires in 1h')) {
        const age = Date.now() - Date.parse(thread.updated_at)
        if (age >= EXPIRES_AFTER_MS) anyExpired = true
      }
    }
    // One refresh for the whole pass, not per row: several threads can
    // cross at once, and this is still the narrow update described above
    // for every row that has not — only a row that has just gone triggers
    // the fetch that redraws the list.
    if (anyExpired) refreshThreads().catch(() => {})
  }

  setInterval(tickWhenLabels, WHEN_TICK_MS)

  // Inline SVG rather than an emoji: an emoji pin ignores `color`, so the
  // pinned state would lose its colour affordance.
  function pinIcon() {
    const ns = 'http://www.w3.org/2000/svg'
    const svg = document.createElementNS(ns, 'svg')
    svg.setAttribute('viewBox', '0 0 24 24')
    svg.setAttribute('width', '14')
    svg.setAttribute('height', '14')
    svg.setAttribute('aria-hidden', 'true')
    const path = document.createElementNS(ns, 'path')
    path.setAttribute('d', 'M16 3v2l-1 1v5l3 3v2h-5v5l-1 1-1-1v-5H6v-2l3-3V6L8 5V3z')
    path.setAttribute('fill', 'currentColor')
    svg.append(path)
    return svg
  }

  // Built node by node rather than from a template string: a thread title
  // is text the reader typed, or text the model wrote, and the transcript
  // above is the only place on this page that is allowed to take markup.
  function threadRow(thread, current) {
    const li = document.createElement('li')
    if (current) li.classList.add('current')

    const label = threadLabel(thread)
    const title = document.createElement('button')
    title.type = 'button'
    title.className = label.unnamed ? 'title unnamed' : 'title'
    title.textContent = label.text
    // The row ellipsises a long name, so the full one has to be reachable.
    title.title = label.text
    // Nothing awaits this, so a network that dies mid-click would be an
    // unhandled rejection; the notice inside is the report that matters.
    title.addEventListener('click', () => { openThread(thread.id).catch(() => {}) })
    li.append(title)

    if (thread.pinned) {
      const pin = document.createElement('span')
      pin.className = 'pin'
      pin.append(pinIcon())
      li.append(pin)
    }

    const when = whenLabel(thread)
    const whenEl = document.createElement('span')
    whenEl.className = when.expiring ? 'when expiring' : 'when'
    whenEl.textContent = when.text
    li.append(whenEl)

    const tools = document.createElement('span')
    tools.className = 'tools'
    tools.append(
      toolButton(pinIcon(), thread.pinned ? 'Unpin' : 'Pin', () => pinThread(thread), thread.pinned),
      toolButton('✎', 'Rename', () => renameInline(li, thread)),
      toolButton('✦', 'Ask Scout for a name', () => suggestTitle(thread)),
      toolButton('✕', 'Delete', () => deleteThread(thread)),
    )
    li.append(tools)
    return li
  }

  function toolButton(glyph, label, onClick, pressed) {
    const b = document.createElement('button')
    b.type = 'button'
    if (typeof glyph === 'string') b.textContent = glyph
    else b.append(glyph)
    b.title = label
    // The glyph is decoration; the label is the only name a screen reader
    // or a hovering cursor can read.
    b.setAttribute('aria-label', label)
    if (pressed !== undefined) b.setAttribute('aria-pressed', String(pressed))
    b.addEventListener('click', (e) => {
      // The whole row opens a thread. Acting on a row must not also switch
      // to it — least of all delete, which would open what it just removed.
      e.stopPropagation()
      // None of these is awaited. Every one of them reports its own failure
      // in the notice, so a throw has nowhere left to go but the console.
      Promise.resolve(onClick()).catch(() => {})
    })
    return b
  }

  // `to` is where focus should land. Closing the drawer on a phone puts it
  // under `display:none`, and the browser answers that by dropping focus to
  // the body — which sends a keyboard reader back to the top of the page.
  function closeDrawer(to) {
    if (sideEl.contains(document.activeElement)) (to ?? menuButton).focus()
    sideEl.classList.remove('open')
    menuButton.setAttribute('aria-expanded', 'false')
  }

  // A 404 from any thread route means the thread went — expired, or
  // deleted on another tab. Refresh the list and show whatever is current.
  //
  // The one place besides `loadHistory` where the server's answer is
  // adopted: the transcript below is fetched from `/chat/history`, which
  // *is* the server's current thread, so the two agree by construction.
  async function vanished() {
    showNotice('That thread is gone. Showing the newest one.')
    await refreshThreads(true)
    try {
      const res = await fetch('/chat/history')
      if (res.ok) showTurns(await res.json())
    } catch {
      // Same as the list: what is on screen is stale, not wrong.
    }
  }

  async function openThread(id) {
    hideNotice()
    const seq = ++openSeq
    const res = await post(`/chat/threads/${id}/open`)
    if (res.status === 404) return vanished()
    if (!res.ok) {
      showNotice('Could not open that thread. Try again.')
      return
    }
    const turns = await res.json()
    // Two rows tapped in a row: the first answer can arrive last, and it
    // would paint its transcript over the thread actually asked for.
    if (seq !== openSeq) return
    showTurns(turns)
    // Whatever a run in the thread just left is that thread's reasoning, and
    // it does not describe the one now on screen.
    hideStatus()
    // Set here rather than left to the refresh below, which never moves it:
    // the composer's target is the transcript now on screen.
    currentThread = id
    closeDrawer(textEl)
    await refreshThreads()
  }

  async function pinThread(thread) {
    hideNotice()
    const res = await post(`/chat/threads/${thread.id}/pin`, { pinned: !thread.pinned })
    if (res.status === 404) return vanished()
    if (!res.ok) showNotice('Could not change that. Try again.')
    await refreshThreads()
  }

  function renameInline(li, thread) {
    const input = document.createElement('input')
    input.value = thread.title ?? ''
    input.maxLength = 80
    input.setAttribute('aria-label', 'Thread name')
    // The row's button and tools give way to the input; the list is
    // rebuilt from the server afterwards, so nothing here is restored by hand.
    li.replaceChildren(input)
    input.focus()
    input.select()
    // Enter, Escape and blur can all arrive for one rename — Enter moves
    // focus off the input, which blurs it. Without this the row would be
    // rebuilt twice and, worse, saved twice.
    let done = false
    const finish = async (save) => {
      if (done) return
      done = true
      const title = input.value.trim()
      try {
        if (save && title && title !== thread.title) {
          const res = await post(`/chat/threads/${thread.id}/rename`, { title })
          if (res.status === 404) return vanished()
          // Core keeps the blank rule, and it is stricter than `trim`: a
          // name made only of invisible characters is not a name.
          if (res.status === 400) showNotice('That name has nothing in it.')
          else if (!res.ok) showNotice('Could not rename that thread. Try again.')
          // A success writes nothing back into `thread`. What core stored is
          // not what was sent — it strips invisible characters, cuts to its
          // own length and trims — so echoing the typed name would show a
          // title the server does not have, and the next refresh would
          // silently correct it. The refresh below is what shows the name.
        }
      } finally {
        // Rebuilt from the thread as it was before the rename, and before
        // the list is asked for: a fetch that fails or never answers must
        // not leave the row as a bare input with no way back out of it. The
        // old name for a moment, and the stored one once the list lands —
        // stale rather than a name that was never saved.
        li.replaceWith(threadRow(thread, thread.id === currentThread))
      }
      await refreshThreads()
    }
    // The listeners do not await it, and the row is put back in the
    // `finally` above whatever happens, so a throw has nothing left to say.
    const settle = (save) => { finish(save).catch(() => {}) }
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') { e.preventDefault(); settle(true) }
      // Held here rather than let through: on a phone this rename is
      // happening inside the open drawer, and the drawer's own Escape
      // would close it — cancelling the rename and the list with it.
      if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); settle(false) }
    })
    input.addEventListener('blur', () => settle(true))
  }

  async function suggestTitle(thread) {
    hideNotice()
    const res = await post(`/chat/threads/${thread.id}/title`)
    if (res.status === 404) return vanished()
    if (res.status === 429) {
      showNotice('Too many names asked for in a row. Give it a few minutes.')
      return
    }
    if (!res.ok) {
      showNotice('Scout could not think of a name. Try again, or rename it yourself.')
      return
    }
    await refreshThreads()
  }

  async function deleteThread(thread) {
    hideNotice()
    const name = threadLabel(thread).text
    if (!window.confirm(`Delete "${name}"? This cannot be undone.`)) return
    const res = await post(`/chat/threads/${thread.id}/delete`)
    // A 404 here is the outcome asked for: the thread is already gone.
    if (!res.ok && res.status !== 404) {
      showNotice('Could not delete that thread. Try again.')
      return
    }
    const wasCurrent = thread.id === currentThread
    // Adopt the server's current when the deleted thread was the one on
    // screen: the reader asked for this, so it is not a thread that "went",
    // and the redraw below is the one this function owns.
    await refreshThreads(wasCurrent)
    if (wasCurrent) {
      const history = await fetch('/chat/history')
      showTurns(history.ok ? await history.json() : [])
    }
  }

  // Returns whether there is now a thread to send into — the composer
  // waits on this, because a message with no thread is a 422.
  async function newThread() {
    hideNotice()
    const res = await post('/chat/threads')
    if (!res.ok) {
      showNotice('Could not start a new thread. Reload to try again.')
      return false
    }
    currentThread = (await res.json()).id
    turnsEl.replaceChildren()
    // An empty thread is an invitation to type into it, so focus lands on
    // the composer rather than back on the menu button.
    closeDrawer(textEl)
    await refreshThreads()
    return true
  }

  menuButton.addEventListener('click', () => {
    const open = sideEl.classList.toggle('open')
    menuButton.setAttribute('aria-expanded', String(open))
    // A drawer opened by keyboard has to put focus inside it, or Tab
    // carries on into the composer behind it.
    if (open) sideEl.querySelector('button')?.focus()
  })
  // The drawer's ways out: Escape, or a tap anywhere else.
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && sideEl.classList.contains('open')) closeDrawer()
  })
  document.addEventListener('click', (e) => {
    if (sideEl.classList.contains('open') && !sideEl.contains(e.target) && !menuButton.contains(e.target)) closeDrawer()
  })
  // A thread renamed on the phone, or one that aged out while this tab sat
  // in the background, should not still be on screen as it was.
  document.addEventListener('visibilitychange', () => {
    // Not adopted: a thread the phone started while this tab slept is the
    // server's current one, and this reader is still looking at theirs.
    // Not awaited either — a tab woken on a dead network is not an error
    // worth a console entry.
    if (document.visibilityState === 'visible') {
      refreshThreads().catch(() => {})
      tripsLoaded = false
      if (!tripsView.hidden) loadTrips().catch(() => {})
    }
  })

  // Runs one turn: posts the question, streams `agent` events into the
  // status line and the answer bubble, and stops on the one `end` frame
  // every stream carries. `EventSource` cannot POST, so the stream is
  // parsed by hand off the `fetch` body reader instead.
  //
  // `retract` takes the "You" bubble the submit handler already appended
  // back off the page. It is called only where the words go back into the
  // composer, so that the message the reader is about to send again is in
  // one place rather than two — see the 422 arm.
  async function runMessage(text, retract = () => {}, fromTrips = false) {
    // The thread this run belongs to. A reader who switches away mid-stream
    // is no longer looking at this conversation, and its tokens must not be
    // painted into the one they moved to. The run carries on server-side
    // and its answer is saved to history, so switching back shows it.
    const runThread = currentThread
    // Whether this run's thread is still the one on screen. The run keeps
    // going server-side either way; the page only draws what belongs to the
    // thread in front of the reader. The status line and the notice are as
    // much this run's output as the bubble is, and a reader who moved to
    // another conversation should see neither its reasoning nor its verdict.
    const mine = () => runThread === currentThread
    let answer = ''
    let thinking = ''
    let answerLi = null
    let sawEnd = false
    // The run behind this answer, named by the first trace frame, and its
    // rows so far — the live panel is redrawn from all of them each time.
    let liveRunId = null
    let liveRows = []
    // A request the server refused outright never opened a stream, so the
    // "connection dropped" report below — which promises the answer is
    // still being written — would be a lie.
    let refused = false

    function renderAnswer() {
      // `showTurns` replaced the transcript under us, so the bubble this
      // run was writing into is off the page. Holding the detached node
      // would write the rest of the answer into nothing.
      if (answerLi && !answerLi.isConnected) answerLi = null
      // Not the thread on screen any more. Neither the bubble nor the
      // scroll belongs to this reader's view.
      //
      // Switching back while the run is still going picks the answer up
      // again from here, but without the question above it: the transcript
      // was redrawn from history, and history does not hold the in-flight
      // turn until the run finishes and writes it. The next open shows both.
      if (runThread !== currentThread) return
      const wasFollowing = following()
      if (!answerLi) {
        answerLi = turnElement('Scout', '')
        turnsEl.append(answerLi)
      }
      answerLi.querySelector('.text').innerHTML = render(answer)
      follow(wasFollowing)
    }

    try {
      const res = await fetch('/chat/messages', {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'x-scout-csrf': csrfToken },
        body: sendBody(text, runThread),
      })
      // Both of these are plain refusals, not streams: the thread went
      // between the page loading and this send, or the page predates the
      // server that now requires a thread. Say which, rather than blaming
      // the connection.
      if (res.status === 404) {
        refused = true
        // Nothing was asked, so the words go back in the box rather than
        // being lost with the thread. `vanished` moves the page to whatever
        // is current, and the next Enter asks the question there.
        textEl.value = text
        fitComposer()
        await vanished()
        return
      }
      if (res.status === 422) {
        refused = true
        // Same: a reload is the advice, and a reader who takes it should
        // find what they typed still in front of them. Nothing here redraws
        // the transcript, though — unlike the 404 above, which hands over to
        // `vanished` — so the bubble the submit handler appended has to come
        // off by hand, or the words sit twice on the page: once in the
        // composer they are going back into, and once in a turn that was
        // never asked.
        retract()
        textEl.value = text
        fitComposer()
        showNotice('This page is out of date. Reload to keep going.')
        return
      }
      if (!res.ok || !res.body) {
        refused = true
        showNotice('Scout could not be reached. Reload to try again.')
        return
      }

      const reader = res.body.getReader()
      const decoder = new TextDecoder()
      let buffer = ''

      const handleFrame = (block) => {
        const frame = parseFrame(block)
        if (!frame) return
        if (frame.event === 'agent') {
          const evt = JSON.parse(frame.data)
          if ('Tool' in evt) {
            if (mine()) showStatus(evt.Tool)
          } else if ('Notice' in evt) {
            if (mine()) showStatus(evt.Notice)
          } else if ('Thinking' in evt) {
            // Accumulated whether or not it is shown: a reader who switches
            // back mid-run should find the reasoning whole, not from here on.
            thinking = applyUpdate(thinking, evt.Thinking)
            if (mine()) showStatus(thinking)
          } else if ('Answer' in evt) {
            answer = applyUpdate(answer, evt.Answer)
            renderAnswer()
          } else if ('Trace' in evt) {
            const f = evt.Trace
            if (f.kind === 'run') {
              liveRunId = f.run_id
            } else {
              // Folded whether or not it is shown, like `thinking`: a
              // reader who switches back mid-run gets the whole trace.
              liveRows = applyTraceFrame(liveRows, f)
              if (debugOn && mine()) {
                const wasFollowing = following()
                renderAnswer()
                renderTracePanel(answerLi, null, liveRows)
                follow(wasFollowing)
              }
            }
          }
        } else if (frame.event === 'end') {
          sawEnd = true
          const end = JSON.parse(frame.data)
          if (end.status === 'busy') {
            if (mine()) showNotice('Scout is already answering something else. Try again in a moment.')
          } else if (end.status === 'error') {
            if (mine()) showNotice(end.message)
          }
          const finished = finalAnswer(end, answer)
          if (finished !== answer) {
            answer = finished
            if (answer !== '' || !answerLi) renderAnswer()
          }
          // An empty bubble is not a cleared one: the turn comes off the
          // page rather than sit blank behind the notice — unless a trace
          // hangs under it, which no history turn will ever carry again.
          // Then the bubble stays to hold it, and says why it is empty.
          if (answer === '' && answerLi) {
            if (keepFailedTurn(end, debugOn, liveRows.length)) {
              answerLi.classList.add('failed')
              answerLi.querySelector('.text').textContent = '(no answer)'
            } else {
              answerLi.remove()
              answerLi = null
            }
          }
          // The answer is written and its trace is saved with results, so
          // the turn takes the button a saved one has — over the live
          // panel, which stays. The run id is kept either way, so a later
          // `/debug on` can give this turn its button too.
          if (typeof liveRunId === 'number' && answerLi && answerLi.isConnected && mine()) {
            answerLi.dataset.runId = String(liveRunId)
            if (debugOn && !answerLi.querySelector('.trace-btn')) {
              answerLi.insertBefore(traceButton(answerLi, liveRunId), answerLi.querySelector('.trace'))
            }
          }
        }
      }

      while (true) {
        const { done, value } = await reader.read()
        if (done) break
        buffer += decoder.decode(value, { stream: true })
        let sep
        while ((sep = buffer.indexOf('\n\n')) !== -1) {
          handleFrame(buffer.slice(0, sep))
          buffer = buffer.slice(sep + 2)
        }
      }
      buffer += decoder.decode()
      if (buffer.trim()) handleFrame(buffer)
    } catch {
      // Fall through to the sawEnd check below — a thrown read is exactly
      // the same situation as one that stopped without an `end` frame.
    } finally {
      hideStatus()
      // A chat turn may have added a segment or parked a flight. The next
      // visit to Trips must read that durable state rather than reuse the
      // snapshot from before the run.
      tripsLoaded = false
      // The reply this run just wrote may have priced a segment or parked
      // a flight against the trip that prompted it. `switchView` already
      // moved the reader to Chat to watch it stream, so refetch now rather
      // than making Trips look stale until they switch back to it by hand.
      if (fromTrips) loadTrips().catch(() => {})
      // Not awaited: the first answer is what names a thread, and the row
      // should pick that name up without holding the composer shut for it.
      // Not adopted either — this run's `save_history` just made its thread
      // the server's current one, and if the reader has since switched
      // away, taking that would move the composer off what they are
      // reading. And not left to reject on its own: a dropped connection
      // here is already reported below.
      refreshThreads().catch(() => {})
      if (!sawEnd && !refused && mine()) {
        // The run continues server-side even though our connection did
        // not, and history is written when it finishes — so this is not
        // an error to apologise for, it's a status to report. And only to
        // the reader still on that thread: to anyone else it is a report
        // about a conversation they are no longer looking at.
        showNotice(
          'The connection dropped before Scout finished. The answer is still being ' +
            'written and will be saved to history — reload to see it once it lands.',
        )
      }
    }
  }

  let running = false

  askForm.addEventListener('submit', async (e) => {
    e.preventDefault()
    if (running) return
    const text = textEl.value.trim()
    if (!text) return
    hideNotice()

    // Captured before anything below moves the page to Chat — once that
    // happens `tripsView.hidden` no longer answers honestly whether this
    // send began on Trips.
    const fromTrips = !tripsView.hidden
    const target = fromTrips ? composerTarget(trips.find((trip) => trip.name === currentTrip)) : null

    // Held from here rather than from the run, because the thread below is
    // made across an await: two quick Enters would otherwise start two.
    running = true
    sendButton.disabled = true
    try {
      if (target) {
        // The trip's own thread is not always the one already open — and
        // for an orphan, a Telegram group, or no trip at all, `target`
        // carries no thread to reuse at all. Either way the transcript on
        // screen has to become the one this message is about to join
        // before the bubble below is appended to it.
        if (target.thread === null) {
          if (!(await newThread())) return
        } else if (target.thread !== currentThread) {
          await openThread(target.thread)
        }
        // So the stream lands where answers already live, not behind the
        // itinerary the reader was just looking at.
        switchView('chat')
      } else if (currentThread === null) {
        // An account with no threads at all — a first sign-in — has
        // nothing to name in the body, and the send would be a 422. Make
        // one before the box is cleared and before the bubble is
        // appended: `newThread` empties the transcript, and it would take
        // that bubble with it.
        if (!(await newThread())) return
      }
      textEl.value = ''
      // A box grown to five lines must shrink back, or it sits tall and
      // empty over the answer it just asked for.
      fitComposer()
      // Held rather than appended and forgotten: a send the server refuses
      // outright never became a turn, and `runMessage` takes the bubble back
      // off in the arm that puts the words back in the composer.
      const youLi = turnElement('You', text)
      turnsEl.append(youLi)
      // Unconditional, unlike the answer: sending is an act that means "show
      // me", so it is not content arriving under a reader who moved away.
      turnsEl.scrollTop = turnsEl.scrollHeight
      // `remove` on a node already detached — a reader who switched threads
      // mid-send had the transcript replaced under them — is a no-op, so
      // this needs no guard of its own.
      await runMessage(text, () => youLi.remove(), fromTrips)
      // The switch may have just flipped, and the turns on screen should
      // show it without a reload.
      if (isDebugCommand(text)) await refreshDebug()
    } finally {
      running = false
      sendButton.disabled = false
    }
  })

  if (mirrorButton) {
    mirrorButton.addEventListener('click', async () => {
      // Read the state off the DOM rather than a variable: the button is
      // the only place it lives, and two copies would disagree the first
      // time a request failed.
      const on = mirrorButton.getAttribute('aria-pressed') !== 'true'
      mirrorButton.disabled = true
      try {
        const res = await fetch('/chat/mirror', {
          method: 'POST',
          headers: { 'content-type': 'application/json', 'x-scout-csrf': csrfToken },
          body: JSON.stringify({ on }),
        })
        if (!res.ok) throw new Error('refused')
        mirrorButton.setAttribute('aria-pressed', String(on))
        showNotice(on ? 'This thread is being sent to Telegram.' : 'No longer sending to Telegram.')
      } catch {
        showNotice('Could not change that. Try again.')
      } finally {
        mirrorButton.disabled = false
      }
    })
  }

  chatTab.addEventListener('click', () => switchView('chat'))
  tripsTab.addEventListener('click', () => switchView('trips'))

  // Through `newThread`, which posts `/chat/threads`: the sidebar has to
  // learn the new thread's id, and the threads route is what hands it back.
  resetForm.addEventListener('submit', async (e) => {
    e.preventDefault()
    await newThread()
  })

  // Nothing awaits the page's first load, and its own failure already
  // shows as a notice — a rejection on top of that is only console noise.
  loadHistory().catch(() => {})
  // Not awaited, and safe either way round: history landing second draws
  // its turns from `debugOn` as it stands, and the switch landing second
  // gives the turns already drawn their buttons through `syncTraceButtons`.
  // `refreshDebug` never rejects.
  refreshDebug()
  // Loaded in the background so the Trips tab can show a count before it is
  // opened. A failure is rendered inside that workspace and does not disturb
  // the chat, which remains the default view.
  loadTrips().catch(() => {})
}

// Guarded so `node --test` can import the pure functions above without a
// document to wire to.
if (typeof document !== 'undefined') {
  start()
}
