import { test } from 'node:test'
import assert from 'node:assert/strict'
import {
  applyUpdate, escapeHtml, finalAnswer, linkify, parseFrame, shouldFollow,
  composerHeight, threadLabel, whenLabel, sendBody, resolveCurrent,
  threadVanished, parseItinerary, selectedCandidate, durationLabel,
  connectionCheck, tripTimelinePoints, tripLoadIsCurrent, savedFareQualifier,
  composerTarget,
} from './chat.js'

test('a Replace clears what was shown rather than extending it', () => {
  // The browser half of the protocol's security property: reasoning the
  // run retracts must leave the screen.
  let answer = ''
  answer = applyUpdate(answer, { Append: 'secret reasoning' })
  answer = applyUpdate(answer, { Replace: '' })
  answer = applyUpdate(answer, { Append: 'The answer' })
  assert.equal(answer, 'The answer')
})

test('an answer is escaped before it reaches the page', () => {
  // The model writes from pages it fetched. This is untrusted text.
  assert.equal(escapeHtml('<img src=x onerror=alert(1)>'), '&lt;img src=x onerror=alert(1)&gt;')
  assert.equal(escapeHtml('5 > 3 & 2 < 4'), '5 &gt; 3 &amp; 2 &lt; 4')
})

test('a url becomes a link and its surroundings stay escaped', () => {
  const out = linkify(escapeHtml('see https://example.com/a?b=1 <b>'))
  assert.match(out, /<a href="https:\/\/example\.com\/a\?b=1"/)
  assert.match(out, /&lt;b&gt;/)
})

test('new content is followed only when the reader is already at the bottom', () => {
  // Following on every update drags a reader back down several times a
  // second while they are trying to re-read a long answer — worse than not
  // following at all, and worst exactly when the answer is worth re-reading.
  assert.equal(shouldFollow(800, 400, 1200), true)

  // A wheel nudge of a line or two still means "I am following along".
  assert.equal(shouldFollow(780, 400, 1200), true)

  // Scrolled up to read. Leave them where they put themselves.
  assert.equal(shouldFollow(300, 400, 1200), false)
})

test('the composer grows with its content and then stops', () => {
  // A box that grows without limit eventually eats the conversation it
  // belongs to.
  assert.equal(composerHeight(40), 40)
  assert.equal(composerHeight(199), 199)
  assert.equal(composerHeight(200), 200)
  assert.equal(composerHeight(4000), 200, 'the cap did not hold')
})

test('the finished answer replaces what the tokens built', () => {
  // The streamed bubble is every turn of a multi-turn run concatenated, so
  // it holds the narration the model writes between tool calls. Only the
  // end frame carries what the run actually answered.
  assert.equal(
    finalAnswer({ status: 'ok', answer: 'EUR 10.99 at bol.com' }, 'Let me check Kruidvat, then bol'),
    'EUR 10.99 at bol.com',
  )
  // Empty means the run produced nothing but reasoning. Clearing the bubble
  // is the right answer, not a bug — there was no answer in it.
  assert.equal(finalAnswer({ status: 'ok', answer: '' }, 'my reasoning'), '')
})

test('an ok end with no answer leaves the bubble alone', () => {
  // Absent is not empty. A server part-way through a rollout still sends
  // `{"status":"ok"}` on its own, and blanking a good reply over a deploy
  // would be worse than showing what streamed.
  assert.equal(finalAnswer({ status: 'ok' }, 'the streamed text'), 'the streamed text')
  // Neither is an end this client cannot read.
  assert.equal(finalAnswer({ status: 'something-new' }, 'the streamed text'), 'the streamed text')
  assert.equal(finalAnswer(null, 'partial'), 'partial')
})

test('a run that failed leaves no half-answer above the apology', () => {
  // Observed: "Good — QP620/50 is in stock at MediaMarkt (27.99 + 2.99
  // verzending). Let me grab the live price" sat directly above "Sorry,
  // something went wrong on my side". That is the narration the model
  // writes between tool calls, and above an apology it reads as an answer
  // cut off mid-sentence rather than as nothing.
  assert.equal(finalAnswer({ status: 'error', message: 'boom' }, 'Let me grab the live price'), '')
  // Busy produced nothing either — the run never started.
  assert.equal(finalAnswer({ status: 'busy' }, ''), '')
})

test('a keep-alive comment is not mistaken for a frame', () => {
  // The server sends comment blocks so a silent run does not look idle to
  // whatever sits between us — a stream that sends nothing at all gets
  // dropped, and the reader is told the connection failed on a run that
  // was going fine. A comment carries no `event:` line.
  assert.equal(parseFrame(':'), null)
  assert.equal(parseFrame(': '), null)
  assert.equal(parseFrame(''), null)
  // And a real frame still parses beside them.
  assert.deepEqual(parseFrame('event: end\ndata: {"status":"ok","answer":"hi"}'), {
    event: 'end',
    data: '{"status":"ok","answer":"hi"}',
  })
})

test('a thread is labelled by its title, or as new when it has none', () => {
  assert.deepEqual(threadLabel({ title: 'wasmiddel per kilo' }), { text: 'wasmiddel per kilo', unnamed: false })
  assert.deepEqual(threadLabel({ title: null }), { text: 'New thread', unnamed: true })
})

test('an empty title is no title', () => {
  // A title that was cleared to '' must read the same as one that was
  // never set — not as a thread named "".
  assert.deepEqual(threadLabel({ title: '' }), { text: 'New thread', unnamed: true })
})

test('a thread says when it was last used, and when it is about to go', () => {
  const now = Date.parse('2026-09-05T12:00:00Z')
  assert.deepEqual(whenLabel({ updated_at: '2026-09-05T11:58:00Z', pinned: false }, now), { text: 'now', expiring: false })
  assert.deepEqual(whenLabel({ updated_at: '2026-09-05T09:30:00Z', pinned: false }, now), { text: '2h', expiring: false })
  assert.deepEqual(whenLabel({ updated_at: '2026-09-04T00:00:00Z', pinned: false }, now), { text: 'expires in 12h', expiring: true })
  // Pinned never expires, however old.
  assert.deepEqual(whenLabel({ updated_at: '2026-09-01T00:00:00Z', pinned: true }, now), { text: '4d', expiring: false })
})

test('whenLabel guards: clock skew and the last hour before expiry', () => {
  const now = Date.parse('2026-09-05T12:00:00Z')
  // Clock skew: an `updated_at` in the future must not go negative or throw.
  assert.deepEqual(whenLabel({ updated_at: '2026-09-05T12:05:00Z', pinned: false }, now), { text: 'now', expiring: false })
  // Exactly 48h old: rounds up to "1h", never "0h" — 0 would read as already gone.
  assert.deepEqual(whenLabel({ updated_at: '2026-09-03T12:00:00Z', pinned: false }, now), { text: 'expires in 1h', expiring: true })
})

test('a date the client cannot read says nothing rather than "NaNd"', () => {
  // Every arithmetic path below runs off `Date.parse`, and NaN propagates
  // silently through all of them — so a row whose timestamp the client
  // cannot parse would render "NaNd" beside its title.
  const now = Date.parse('2026-09-05T12:00:00Z')
  assert.deepEqual(whenLabel({ updated_at: 'garbage', pinned: false }, now), { text: '', expiring: false })
})

test('a message names the thread it belongs to', () => {
  assert.deepEqual(JSON.parse(sendBody('hi', 42)), { text: 'hi', thread: 42 })
})

test('a list refresh does not move the composer off the thread on screen', () => {
  // The race `MessageIn.thread` exists to close. The server's `current` is
  // whichever thread was touched last *anywhere* — the phone, another tab,
  // a run in another thread that just finished and wrote its history. The
  // reader is looking at 2; the next message belongs in 2.
  const list = [{ id: 1, current: true }, { id: 2 }]
  assert.equal(resolveCurrent(list, 2), 2)
})

test('a thread that has gone from the list hands the composer to the server', () => {
  // Expired, or deleted on the phone. There is no transcript to protect any
  // more, so the server's answer is the only one left.
  assert.equal(resolveCurrent([{ id: 1, current: true }], 2), 1)
})

test('a page with no thread of its own takes the server\'s', () => {
  assert.equal(resolveCurrent([{ id: 1, current: true }, { id: 2 }], null), 1)
})

test('the openers that redraw the transcript adopt the server\'s answer', () => {
  // `loadHistory` and `vanished` both render `/chat/history` — which *is*
  // the server's current thread — so there the server's answer and what is
  // on screen are the same thing, and adopting it is right.
  assert.equal(resolveCurrent([{ id: 1, current: true }, { id: 2 }], 2, true), 1)
})

test('a thread that went while the tab slept is noticed, not quietly swapped', () => {
  // The 48h sweep runs while the tab is in the background, and the wake-up
  // refresh is the first thing to see the thread gone. Without this the
  // composer would retarget under an unchanged transcript and the next
  // message would land in a conversation the reader never opened.
  assert.equal(threadVanished([{ id: 1, current: true }], 2), true)
  // Still there: nothing happened.
  assert.equal(threadVanished([{ id: 1, current: true }, { id: 2 }], 2), false)
  // Nothing on screen to lose — a first load, before any transcript.
  assert.equal(threadVanished([{ id: 1, current: true }], null), false)
  // The adopting callers have just drawn `/chat/history` themselves, so the
  // thread they are moving to is by construction the server's current one.
  // Reporting it gone there would announce the move that was just made —
  // and, since `vanished` refreshes with `adopt`, would never terminate.
  assert.equal(threadVanished([{ id: 1, current: true }], 2, true), false)
})

test('an account with no threads at all leaves the composer with none', () => {
  // Not `undefined`: the composer tests `currentThread === null` to decide
  // whether to make a thread before sending.
  assert.equal(resolveCurrent([], 7), null)
  assert.equal(resolveCurrent([], null), null)
  // A list where nothing is marked current — `threads` reads the list and
  // the current id in two statements, so a thread can vanish between them.
  assert.equal(resolveCurrent([{ id: 1 }, { id: 2 }], null), null)
})

test('a stored itinerary becomes airports, times, and connection waits', () => {
  assert.deepEqual(
    parseItinerary('AMS 20:15 15.09 ✈ PVG 3h 20m ✈ HKG 20:35 16.09'),
    [
      { airport: 'AMS', departsFrom: null, time: '20:15', date: '15.09', wait: null },
      { airport: 'PVG', departsFrom: null, time: null, date: null, wait: '3h 20m' },
      { airport: 'HKG', departsFrom: null, time: '20:35', date: '16.09', wait: null },
    ],
  )
  assert.deepEqual(parseItinerary('JFK ✈ LGA 4h'), [
    { airport: 'JFK', departsFrom: null, time: null, date: null, wait: null },
    { airport: 'LGA', departsFrom: null, time: null, date: null, wait: null },
  ])
})

test('an airport-changing stop keeps both airports visible', () => {
  assert.deepEqual(parseItinerary('HND 09:00 12.10 ✈ NRT/HND 5h 10m ✈ CTS 18:00 12.10')[1], {
    airport: 'NRT', departsFrom: 'HND', time: null, date: null, wait: '5h 10m',
  })
})

test('one candidate is selected by elimination and several require a choice', () => {
  const only = { candidate: 1, chosen: false }
  assert.equal(selectedCandidate({ candidates: [only] }), only)
  assert.equal(selectedCandidate({ candidates: [{ candidate: 1 }, { candidate: 2 }] }), null)
  assert.equal(selectedCandidate({ candidates: [{ candidate: 1 }, { candidate: 2, chosen: true }] }).candidate, 2)
})

test('durations are formatted without dropping minutes', () => {
  assert.equal(durationLabel(50), '50m')
  assert.equal(durationLabel(180), '3h')
  assert.equal(durationLabel(201), '3h 21m')
  assert.equal(durationLabel(8150), '5d 15h 50m')
  assert.equal(durationLabel(null), 'Duration unavailable')
})

test('the join between selected flights is checked on the shared local clock', () => {
  const candidate = (departure, arrival, chosen = true) => ({
    chosen,
    departing_at_local: departure,
    arriving_at_local: arrival,
  })
  const before = {
    destination: 'LIS',
    candidates: [candidate('2026-10-12T08:00:00', '2026-10-12T10:00:00')],
  }
  const comfortable = {
    origin: 'LIS',
    candidates: [candidate('2026-10-12T14:20:00', '2026-10-12T17:00:00')],
  }
  const tight = {
    origin: 'LIS',
    candidates: [candidate('2026-10-12T11:15:00', '2026-10-12T14:00:00')],
  }
  assert.deepEqual(connectionCheck(before, comfortable), {
    tone: 'ready', text: '4h 20m at LIS between the selected flights.',
  })
  assert.deepEqual(connectionCheck(before, tight), {
    tone: 'danger', text: '1h 15m at LIS — tight connection; allow at least 3 hours between separate tickets.',
  })
})

test('a change of airport is reported instead of subtracting unrelated clocks', () => {
  const before = {
    destination: 'FCO',
    candidates: [{ chosen: true, arriving_at_local: '2026-10-12T10:00:00' }],
  }
  const after = {
    origin: 'FLR',
    candidates: [{ chosen: true, departing_at_local: '2026-10-12T16:00:00' }],
  }
  assert.deepEqual(connectionCheck(before, after), {
    tone: 'warning',
    text: 'Airport transfer: arrive at FCO, continue from FLR. Travel between them is not included.',
  })
})

test('the trip timeline includes layover airports and makes route gaps visible', () => {
  const points = tripTimelinePoints({ segments: [
    {
      origin: 'AMS', destination: 'HKG', departure_date: '2026-10-12',
      candidates: [{ chosen: true, itinerary: 'AMS 08:00 12.10 ✈ PVG 3h 20m ✈ HKG 20:00 13.10' }],
    },
    {
      origin: 'NRT', destination: 'SFO', departure_date: '2026-10-18',
      candidates: [{ chosen: true, itinerary: 'NRT 09:00 18.10 ✈ SFO 02:00 18.10' }],
    },
  ] })

  assert.deepEqual(points, [
    { code: 'AMS', date: '2026-10-12', gap: false },
    { code: 'PVG', date: '3h 20m', gap: false },
    { code: 'HKG', date: '', gap: true },
    { code: 'NRT', date: '2026-10-18', gap: false },
    { code: 'SFO', date: '', gap: false },
  ])
})

test('a stale trip read cannot repaint a newer choice', () => {
  assert.equal(tripLoadIsCurrent(4, 4, false), true)
  assert.equal(tripLoadIsCurrent(3, 4, false), false, 'an older GET response was accepted')
  assert.equal(tripLoadIsCurrent(4, 4, true), false, 'a GET raced a pending selection')
})

test('Ignav saved fares stay visibly approximate', () => {
  assert.deepEqual(savedFareQualifier('ignav'), { prefix: 'from ', note: 'estimate when saved' })
  assert.deepEqual(savedFareQualifier('duffel'), { prefix: '', note: 'when saved' })
})

test('the composer says which thread a trip message lands in', () => {
  // Three cases, because a trip's owner is not always somewhere the web
  // can post: an owned direct thread, an orphan, and a Telegram group.
  assert.deepEqual(composerTarget({ chat: { id: 7, title: 'Cheap flights in October', scope: 'direct' } }),
    { thread: 7, label: 'to "Cheap flights in October"' })

  assert.deepEqual(composerTarget({ chat: { id: 7, title: null, scope: 'direct' } }),
    { thread: 7, label: 'to an unnamed thread' })

  // Orphaned: sending starts a thread, which then adopts the trip.
  assert.deepEqual(composerTarget({ chat: null }),
    { thread: null, label: 'to a new chat' })

  // A group is a room with other people in it. Offer a new direct thread
  // instead, and do not take the group's ownership away from it.
  assert.deepEqual(composerTarget({ chat: { id: 9, title: 'Trip crew', scope: 'telegram:-100' } }),
    { thread: null, label: 'planned in a Telegram group — replies go to a new chat' })

  // No trip is on screen at all — the empty state, or a selection that
  // hasn't resolved yet. There is nothing to name, so the line says
  // nothing (the caller hides it), and a send still gets a thread of its
  // own rather than reusing whatever thread the page last had open in
  // Chat, which has nothing to do with whatever gets typed here.
  assert.deepEqual(composerTarget(undefined), { thread: null, label: '' })
})
