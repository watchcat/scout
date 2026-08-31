import { test } from 'node:test'
import assert from 'node:assert/strict'
import { applyUpdate, escapeHtml, linkify, shouldFollow, composerHeight } from './chat.js'

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
