import { test } from 'node:test'
import assert from 'node:assert/strict'
import { applyUpdate, escapeHtml, linkify } from './chat.js'

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
