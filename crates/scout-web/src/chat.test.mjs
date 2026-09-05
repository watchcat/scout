import { test } from 'node:test'
import assert from 'node:assert/strict'
import { applyUpdate, escapeHtml, finalAnswer, linkify, parseFrame, shouldFollow, composerHeight, threadLabel, whenLabel, sendBody } from './chat.js'

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
