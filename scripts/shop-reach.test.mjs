// Tests for shop-reach.mjs: node --test scripts/shop-reach.test.mjs
// The parts that decide something without a browser or a model.
import test from 'node:test'
import assert from 'node:assert/strict'
import {
  textWritesAmount, site, sameSite, typingRefused, refusedBy, checkReport, gained, differs, parseEnv,
  trimHistory, tables, ruleMet, MAX_TYPED,
} from './shop-reach.mjs'

test('an amount is written with a point or a comma, and not as part of a longer number', () => {
  assert.ok(textWritesAmount('Nu voor € 12,99 per stuk', 12.99))
  assert.ok(textWritesAmount('Price: 12.99 EUR', 12.99))
  assert.ok(!textWritesAmount('was 112,99', 12.99))
  assert.ok(!textWritesAmount('12,995 sold', 12.99))
  assert.ok(textWritesAmount('€ 349,00', 349))
  assert.ok(!textWritesAmount('349 reviews', 349))
  assert.ok(!textWritesAmount('anything', null))
  assert.ok(!textWritesAmount('anything', 'twelve'))
})

test('free is written in words, so zero is confirmed by one of them', () => {
  assert.ok(textWritesAmount('Gratis verzending vanaf 20 euro', 0))
  assert.ok(textWritesAmount('Kostenloser Versand', 0))
  assert.ok(textWritesAmount('Free delivery', 0))
  assert.ok(textWritesAmount('Verzendkosten € 0,00', 0))
  assert.ok(!textWritesAmount('Verzendkosten € 4,95', 0))
})

test('a shop is its registrable domain, whatever stands in front of it', () => {
  assert.equal(site('www.shop.example'), 'shop.example')
  assert.equal(site('checkout.shop.example'), 'shop.example')
  assert.equal(site('www.shop.co.uk'), 'shop.co.uk')
  assert.equal(site('shop.com.hk'), 'shop.com.hk')
  assert.ok(sameSite('https://www.shop.example/p/1', 'https://basket.shop.example/'))
  assert.ok(!sameSite('https://www.shop.example/p/1', 'https://pay.example/shop.example'))
  assert.ok(!sameSite('https://www.shop.example/p/1', 'not an address'))
})

test('what may be typed is a search term, a quantity, a postcode or a house number', () => {
  const ok = (field, text = '1011 PN') => assert.equal(typingRefused(field, text), null, JSON.stringify(field))
  const no = (field, text = 'x') => assert.ok(typingRefused(field, text), JSON.stringify(field))
  ok({ type: 'text', name: 'postcode' })
  ok({ type: 'text', label: 'Postcode', autocomplete: 'postal-code' })
  ok({ type: 'search', placeholder: 'Zoeken' }, 'kattenvoer')
  ok({ type: 'number', name: 'quantity' }, '2')
  ok({ type: 'text', label: 'Huisnummer' }, '1')
  no({ type: 'password' })
  no({ type: 'email' })
  no({ type: 'tel' })
  no({ type: 'text', autocomplete: 'cc-number' })
  no({ type: 'text', autocomplete: 'given-name' })
  no({ type: 'text', autocomplete: 'street-address' })
  no({ type: 'text', name: 'email_address' })
  no({ type: 'text', label: 'Voornaam' })
  no({ type: 'text', placeholder: 'Straatnaam' })
  no({ type: 'text', name: 'iban' })
  no({ type: 'text', label: 'Telefoonnummer' })
  // Long text is never a postcode.
  no({ type: 'text', name: 'postcode' }, 'x'.repeat(MAX_TYPED + 1))
  ok({ type: 'text', name: 'postcode' }, 'x'.repeat(MAX_TYPED))
})

test('a wall is told by its status or by its title, and a page that is short and asks who you are', () => {
  assert.ok(refusedBy(403, 'Shop', 'x'))
  assert.ok(refusedBy(429, 'Shop', 'x'))
  assert.ok(refusedBy(200, 'Just a moment...', 'x'))
  assert.ok(refusedBy(200, 'Shop', 'Enter the characters you see below. Sorry, we just need to make sure you are not a robot.'))
  assert.ok(!refusedBy(200, 'Shop', 'A long product page. '.repeat(200) + 'captcha'))
  assert.ok(!refusedBy(200, 'Widget kopen', 'Widget 12,99'))
  assert.ok(!refusedBy(404, 'Niet gevonden', 'x'))
})

test('a report is checked against what the session was shown, amount by amount', () => {
  const seen = ['Widget € 12,99 Kies maat', 'Winkelmand Widget XL € 14,99 Verzendkosten € 4,95 Bezorging: morgen in huis']
  const c = checkReport({ price: 12.99, variant_price: 14.99, shipping: 4.95, delivery: 'Morgen in huis' }, seen)
  assert.deepEqual(c, { price: true, variant_price: true, shipping: true, delivery: true })
  const d = checkReport({ price: 11.99, shipping: 0, delivery: 'binnen 2 werkdagen' }, seen)
  assert.deepEqual(d, { price: false, variant_price: null, shipping: false, delivery: false })
  // A model quotes with quotation marks and a full stop; the page has neither.
  assert.equal(checkReport({ delivery: '"Morgen in huis".' }, seen).delivery, true)
  assert.equal(checkReport({ delivery: '“morgen  in huis”' }, seen).delivery, true)
  // A sentence about the wording is not the wording.
  assert.equal(checkReport({ delivery: 'The page says morgen in huis, on the basket page' }, seen).delivery, false)
})

test('where a session read another number than a read did, that is said, not counted as a gain', () => {
  const plain = { price: 5.94 }, render = { price: 5.94 }
  assert.deepEqual(differs({ price: 6.99, shipping: 4.99 }, [plain, render]), ['price'])
  assert.deepEqual(differs({ price: 5.94 }, [plain, render]), [])
  assert.deepEqual(differs({ price: 6.99 }, [null, { price: 6.99 }, { price: 5.94 }]), [])
  assert.deepEqual(gained({ price: 6.99, shipping: 4.99 }, [plain, render]), ['shipping'])
})

test('what a session gained is a number it read that neither other way had', () => {
  const plain = { price: 12.99 }, render = { price: 12.99, delivery: 'morgen' }
  const drive = { price: 12.99, variant_price: 14.99, shipping: 4.95, delivery: 'morgen' }
  assert.deepEqual(gained(drive, [plain, render]), ['variant_price', 'shipping'])
  assert.deepEqual(gained({ price: 12.99 }, [plain, render]), [])
  assert.deepEqual(gained({ price: 12.99 }, [null, null]), ['price'])
  // A delivery wording is not a number; it is shown in the table and not counted.
  assert.deepEqual(gained({ delivery: 'morgen' }, [null, null]), [])
})

test('the .env is read without its quotes and without its comments', () => {
  const env = parseEnv('# a comment\nA=1\nB="two words"\nC=\n D = spaced \nnot a line\n')
  assert.deepEqual(env, { A: '1', B: 'two words', C: '', D: 'spaced' })
})

test('older pages leave the conversation, the last two stay whole', () => {
  const page = (n) => ({ role: 'tool', tool_call_id: `c${n}`, content: `PAGE ${n}\nTitle ${n}\nhttps://shop.example/${n}\n` + 'text '.repeat(50) })
  const history = [{ role: 'system', content: 's' }, { role: 'user', content: 'u' }, page(1), page(2), page(3)]
  const trimmed = trimHistory(history, 2)
  assert.equal(trimmed.length, 5)
  assert.ok(trimmed[2].content.length < 120 && trimmed[2].content.includes('Title 1'), trimmed[2].content)
  assert.equal(trimmed[3].content, history[3].content)
  assert.equal(trimmed[4].content, history[4].content)
  assert.equal(trimmed[2].tool_call_id, 'c1')
  assert.equal(history[2].content.length > 200, true, 'the original is left alone')
})

const result = (label, host, isPublic, drive, plain = null, render = null) => ({
  label, host, public: isPublic,
  plain: { status: 200, report: plain, verified: plain ? { price: true } : {} },
  render: { status: 200, report: render, verified: render ? { price: true } : {} },
  drive: { refused: false, report: drive, verified: Object.fromEntries(Object.keys(drive ?? {}).map((k) => [k, true])), turns: 5, actions: 7, prompt_tokens: 12000, completion_tokens: 800, seconds: 61.4, stopped: 'reported' },
  links: { variant: 'in the address', cart: null },
})

test('the tables name a host only when it is public, and letter the rest', () => {
  const rows = [
    result('a', 'shared.example', true, { price: 10, shipping: 4.95 }, { price: 10 }, { price: 10 }),
    result('b', 'one-person.example', false, { price: 20 }, { price: 20 }, { price: 20 }),
    result('c', 'another-person.example', false, { price: 30, variant_price: 35 }),
  ]
  const text = tables(rows)
  assert.ok(text.includes('shared.example'))
  assert.ok(!text.includes('one-person.example') && !text.includes('another-person.example'))
  assert.ok(text.includes('shop E') && text.includes('shop F'), 'unnamed rows are lettered after the four named hosts')
  // Everything, for the operator.
  assert.ok(tables(rows, { nameAll: true }).includes('one-person.example'))
  // A price the session read otherwise than the read did is flagged in the second table.
  rows[0].drive.report.price = 12
  assert.match(tables(rows), /shared\.example \| shipping \| price \|/)
  // The driven row carries what the session cost.
  assert.match(text, /5 turns/)
  assert.match(text, /12,800 tokens/)
  assert.match(text, /61 s/)
})

test('the rule: on three hosts a session reads a number the other ways did not, unrefused', () => {
  const yes = (label) => result(label, `${label}.example`, true, { price: 10, shipping: 4.95 }, { price: 10 }, { price: 10 })
  const no = (label) => result(label, `${label}.example`, true, { price: 10 }, { price: 10 }, { price: 10 })
  assert.deepEqual(ruleMet([yes('a'), yes('b'), no('c')]), { hosts: 2, met: false })
  assert.deepEqual(ruleMet([yes('a'), yes('b'), yes('c'), no('d')]), { hosts: 3, met: true })
  // A number the session reported and its pages did not write does not count.
  const unverified = yes('e'); unverified.drive.verified.shipping = false
  assert.deepEqual(ruleMet([yes('a'), yes('b'), unverified]), { hosts: 2, met: false })
  // Nor does one from a shop that refused the session.
  const refused = yes('f'); refused.drive.refused = true
  assert.deepEqual(ruleMet([yes('a'), yes('b'), refused]), { hosts: 2, met: false })
})
