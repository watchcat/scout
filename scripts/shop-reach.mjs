#!/usr/bin/env node
// What a browser can reach on a shop that a read cannot. Gate 2 of
// docs/superpowers/specs/2026-11-17-price-watch-and-shop-browsing-design.md.
//
// For each product page in a list, three ways of reading it, all leaving
// from the production node's address (the one shops see):
//
//   plain   the request fetch_page makes, by Scout's own client;
//   render  the one-shot headless render fetch_page falls back to;
//   drive   a session a model drives: open, read, click, type, select, back.
//
// The first two are judged by Scout's extractor itself
// (crates/scout-core/examples/page_as_read.rs), and then the model is handed
// what fetch_page would have handed it and asked the same four things the
// session is asked: the price, the price of another variant, what delivery
// to a Dutch postcode costs, and when it comes. An amount counts only where
// the text the model was shown writes it.
//
// The session is a throwaway prototype of the spec's loop, with its rules:
// one shop, nothing personal typed, no sign-in, no bot check attempted,
// twelve turns, 25 actions, 120 seconds. It is not the thing to build; it is
// what tells whether to. Where it keeps a rule less well than the spec asks:
//
// - the seconds are checked between turns, so a slow model call overruns;
// - only a link is refused for leaving the shop before it is pressed; a
//   redirect or a script is followed and then undone, and a new tab is not
//   seen at all;
// - a shop is told by its last two labels, which a shared suffix defeats;
// - nothing refuses a checkout or a sign-in address but the brief;
// - a press lands where the thing is, and something lying over it gets it.
//
//   ssh -N -D 127.0.0.1:18080 $SCOUT_SSH &          # leave by the node
//   cargo build -p scout-core --example page_as_read
//   scripts/shop-reach.mjs read products.json out/ [--only label] [--ways plain,render,drive]
//   scripts/shop-reach.mjs table out/ [--name-all]
//   scripts/shop-reach.mjs recheck out/      # the checks again, over the pages already saved
//   scripts/shop-reach.mjs follow <address>... [--needle text]...
//
// Keep products.json and out/ outside the repository: they name hosts.
//
// `follow` opens addresses one after another in a single session and says
// where each landed and whether the page wrote each needle: how a link that
// claims to pick a variant or fill a basket is checked.
//
// products.json: [{"label": "a", "host": "shop.example", "public": true,
// "url": "https://shop.example/p/1"}]. `public` says whether the host may be
// named in a file anyone can read: only where two or more accounts used it
// (scripts/unread-pages.py). The rest are lettered in the tables.
//
// The model is Scout's own (`SHOP_MODEL`, else `LLM_MODEL`, read from .env
// with its key). No dependencies: Node 22's fetch and WebSocket, the Chrome
// on this machine. Tests: node --test scripts/shop-reach.test.mjs
import { spawn, execFile } from 'node:child_process'
import { promisify } from 'node:util'
import net from 'node:net'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const run = promisify(execFile)
const HERE = path.dirname(fileURLToPath(import.meta.url))
const ROOT = path.dirname(HERE)
const EXTRACTOR = path.join(ROOT, 'target/debug/examples/page_as_read')
export const CHROME = process.env.SCOUT_CHROME || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'

export const MAX_TYPED = 64
const OUTLINE_CHARS = 6000
const RENDER_TIMEOUT_MS = 45_000       // tools/browser.rs, RENDER_TIMEOUT
const VIRTUAL_TIME_BUDGET_MS = 30_000  // tools/browser.rs
const POSTCODE = '1011 PN'             // Amsterdam's city hall: an address that is nobody's
const NUMBERS = ['price', 'variant_price', 'shipping']

// ---------------------------------------------------------------- deciding

// Free, said of delivery: the two words within a clause of each other, or
// one of the compounds that say both. "Gratis retourneren" alone is not it.
// It still takes "free above an amount" for free, and a returns line beside
// a delivery line: a zero it confirms wants reading, as the write-up did.
const FREE = '(?:gratis|kostenlos\\w*|kostenfrei\\w*|free)'
const DELIVERY = '(?:verzend\\w*|bezorg\\w*|lever\\w*|versand\\w*|liefer\\w*|deliver\\w*|shipping)'
const FREE_DELIVERY = new RegExp(
  `\\b${FREE}\\b[^.\\n]{0,30}?\\b${DELIVERY}|\\b${DELIVERY}[^.\\n]{0,30}?\\b${FREE}\\b|\\b${DELIVERY}[^.\\n]{0,30}?(?<![0-9.,])0[.,]00(?![0-9.,])` +
  '|gratisversand|versandkostenfrei|keine versandkosten|geen verzendkosten', 'i')

/// The extractor's rule (`text_writes_amount`, tools/fetch.rs): "12.99" or
/// "12,99", not a piece of a longer number. Wider than it in two ways a
/// shop's visible page needs and embedded data does not: a whole amount is
/// also "259,-" or "€ 259", and zero, which nobody writes as a number worth
/// trusting, is confirmed by a word for free beside a word for delivery.
export function textWritesAmount(text, amount) {
  if (typeof amount !== 'number' || !Number.isFinite(amount)) return false
  if (amount === 0) return FREE_DELIVERY.test(text)
  const point = amount.toFixed(2)
  const escape = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
  if ([point, point.replace('.', ',')].some((needle) => new RegExp(`(?<![0-9.,])${escape(needle)}(?![0-9.,])`).test(text))) return true
  if (!Number.isInteger(amount)) return false
  const n = String(amount)
  const alone = `(?<![0-9.,])${n}`
  return new RegExp(`${alone}[.,][-–—]`).test(text) ||
    new RegExp(`(?:€|EUR)\\s?${n}(?![0-9]|[.,][0-9])`).test(text) && new RegExp(`(?:€|EUR)\\s?${alone}`).test(text) ||
    new RegExp(`${alone}\\s?(?:€|EUR)`).test(text)
}

/// Whether two wordings name the same option: a model writes a hyphen where
/// the page has a dash, and drops a colon.
export function sameOption(a, b) {
  const flat = (s) => String(s).toLowerCase().replace(/×/g, 'x').replace(/[^\p{L}\p{N}]/gu, '')
  return flat(a) !== '' && flat(a) === flat(b)
}

const TWO_LABEL_SUFFIXES = new Set(['co.uk', 'com.hk', 'co.il', 'com.au', 'co.nz', 'com.tr', 'co.jp', 'com.br'])

/// A shop's registrable domain. Good enough for a list of shops someone chose.
export function site(hostname) {
  const labels = String(hostname || '').toLowerCase().replace(/\.$/, '').split('.')
  const keep = TWO_LABEL_SUFFIXES.has(labels.slice(-2).join('.')) ? 3 : 2
  return labels.slice(-keep).join('.')
}

export function sameSite(a, b) {
  try {
    return site(new URL(a).hostname) === site(new URL(b).hostname)
  } catch {
    return false
  }
}

const POSTCODE_FIELD = /(post.?code|postal.?code|\bzip|plz)/
const COUNTED = /(aantal|anzahl|hoeveelheid|quantity|qty|menge|stück|stuks|huisnummer|hausnummer|house.?number)/
const PLACE = /^(adres|address|ort|stadt|city|plaats)$/
const PERSONAL = /(naam|name|mail|telefo|\btel\b|gsm|rufnummer|phone|handy|mobil|straat|street|stra(?:ß|ss)e|adres|address|\bort\b|wohnort|stadt|city|town|plaats|gemeente|card|kaart|karte|iban|konto|rekening|account|bsn|cvv|cvc|prüfziffer|sicherheitscode|security.?code|wachtwoord|password|passwort|kennwort|\bpin\b|geboorte|geboren|geburt|birth|\bdob\b|login|benutzer|user|kunden|klant|customer|firma|company|bedrijf)/g
const PERSONAL_AUTOCOMPLETE = /^(name|given-name|additional-name|family-name|nickname|honorific-.*|email|tel(-.*)?|street-address|address-line\d|address-level\d|username|new-password|current-password|one-time-code|bday(-.*)?|organization(-title)?|cc-.*|sex|url|photo)$/

/// Why a field may not be typed into, or null when it may. Anything a page
/// marks as personal is refused before anything is allowed; what is left is
/// a search term, a quantity, a postcode, a house number. (The house number
/// is one more than the spec lists: a Dutch postcode is asked for with it.)
export function typingRefused(field, text) {
  if (String(text).length > MAX_TYPED) return `at most ${MAX_TYPED} characters may be typed`
  const type = (field.type || 'text').toLowerCase()
  if (['password', 'email'].includes(type)) return `a ${type} field is never typed into`
  const auto = (field.autocomplete || '').toLowerCase().split(/\s+/)
  if (auto.some((token) => PERSONAL_AUTOCOMPLETE.test(token))) return 'that field is for something personal'
  // A search box is one whatever it suggests looking for.
  if (type === 'search') return null
  const said = [field.name, field.id, field.placeholder, field.label, field.title].filter(Boolean).join(' ').toLowerCase()
  // A postcode field inside an address form, or one that takes a place as well, is still a postcode field.
  const personal = (said.match(PERSONAL) || []).filter((word) => !(POSTCODE_FIELD.test(said) && PLACE.test(word)))
  if (personal.length) return 'that field is for something personal'
  // A numeric keypad is for a postcode or a quantity, or it is for a phone number.
  if (type === 'tel' && !POSTCODE_FIELD.test(said) && !COUNTED.test(said)) return 'a tel field is typed into only for a postcode or a quantity'
  return null
}

const WALL_TITLES = ['just a moment', 'checking your browser', 'attention required', 'access denied', 'please wait for verification', 'verifying you are human']
const WALL_WORDS = /captcha|not a robot|geen robot|kein roboter|are you (a )?human|verify(ing)? you are human|unusual traffic|toegang geweigerd|access denied|zugriff verweigert/i

/// Whether a page is a refusal: by its status, by a title Scout already
/// knows (tools/browser.rs), or a short page that asks who is there.
export function refusedBy(status, title, text) {
  if ([401, 403, 429, 503].includes(status)) return true
  const t = String(title || '').toLowerCase()
  if (WALL_TITLES.some((m) => t.includes(m))) return true
  return String(text || '').length < 1500 && WALL_WORDS.test(text || '')
}

/// Why a session ends without asking the model again, or null. A model told
/// to stop at a refusal mostly does; once it went on opening pages of a shop
/// that had blocked it, so the loop does not leave it the choice.
export function turnedAway({ firstWall, walls, wallsInARow }) {
  if (firstWall) return 'the product page was refused'
  if (wallsInARow >= 2) return 'the shop refused two pages running'
  if (walls >= 3) return 'the shop refused three pages'
  return null
}

/// Which of a report's findings the shown text writes. null: not reported.
export function checkReport(report, texts) {
  const seen = texts.join('\n')
  const flat = seen.toLowerCase().replace(/\s+/g, ' ')
  const bare = (s) => s.replace(/\([^)]*\)?/g, ' ').replace(/\s+/g, ' ')
  const out = {}
  const number = (v) => (typeof v === 'number' ? v : typeof v === 'string' && v.trim() !== '' && Number.isFinite(Number(v)) ? Number(v) : null)
  for (const key of NUMBERS) {
    const amount = number(report?.[key])
    // Nothing costs less than nothing, and only delivery is ever free.
    out[key] = amount === null ? null : amount < 0 || (amount === 0 && key !== 'shipping') ? false : textWritesAmount(seen, amount)
  }
  // The wording as quoted: one quotation among several is enough, if it is
  // long enough to be a wording and not a word.
  const quoted = report?.delivery
    ? bare(String(report.delivery).toLowerCase()).split(/["“”„‟«»‹›]|(?:^|\s)['‘’‚]|['‘’](?:\s|$|[.,;])|\s\/\s|;/).map((q) => q.replace(/^[\s.]+|[\s.]+$/g, '')).filter((q) => q.length >= 8)
    : null
  // Asides are dropped from the page too, so a wording quoted with or without its own matches.
  out.delivery = quoted ? quoted.some((q) => flat.includes(q) || bare(flat).includes(q)) : null
  return out
}

/// A report with only what its pages wrote.
function confirmed(way) {
  if (!way?.report) return null
  const out = {}
  for (const [key, ok] of Object.entries(way.verified || {})) if (ok === true) out[key] = way.report[key]
  return out
}

/// The numbers a session read that no other way had.
export function gained(drive, others) {
  return NUMBERS.filter((key) => typeof drive?.[key] === 'number' && !others.some((o) => typeof o?.[key] === 'number'))
}

/// The numbers a session read otherwise than every other way that read them.
export function differs(drive, others) {
  return NUMBERS.filter((key) => {
    const theirs = others.map((o) => o?.[key]).filter((v) => typeof v === 'number')
    return typeof drive?.[key] === 'number' && theirs.length > 0 && theirs.every((v) => Math.abs(v - drive[key]) > 0.004)
  })
}

/// The "possible" half of the spec's rule: on at least three hosts a driven
/// session gets a number the other two ways do not, and is not refused.
export function ruleMet(rows) {
  const hosts = rows.filter((r) => !r.drive?.refused && gained(confirmed(r.drive), [confirmed(r.plain), confirmed(r.render)]).length > 0).length
  return { hosts, met: hosts >= 3 }
}

export function parseEnv(text) {
  const env = {}
  for (const line of text.split('\n')) {
    const m = /^\s*([A-Za-z0-9_]+)\s*=\s*(.*?)\s*$/.exec(line)
    if (!m || line.trim().startsWith('#')) continue
    env[m[1]] = m[2].replace(/^(['"])(.*)\1$/, '$2')
  }
  return env
}

/// The conversation with every page but the last `keep` cut to its title and
/// address: a page read four turns ago is not worth its tokens again.
export function trimHistory(history, keep) {
  const pages = history.map((m, i) => (m.role === 'tool' && String(m.content).startsWith('PAGE ') ? i : -1)).filter((i) => i >= 0)
  const old = new Set(pages.slice(0, Math.max(0, pages.length - keep)))
  return history.map((m, i) => {
    if (!old.has(i)) return m
    const [, title = '', address = ''] = String(m.content).split('\n')
    return { ...m, content: `(an earlier page, no longer shown: ${title.slice(0, 40)} ${address.slice(0, 60)})` }
  })
}

// ------------------------------------------------------------------ tables

const money = (n) => (typeof n === 'number' ? n.toFixed(2) : '—')

function cells(name, way) {
  if (!way || way.failed) return ['—', '—', '—', '—']
  const { report, verified = {} } = way
  // A session is refused when the loop says so; a side page that would not
  // open is not that. A fetched page the model called a refusal has nothing to show.
  if (!report || (name === 'drive' ? way.refused : report.refused)) return ['—', '—', '—', '—']
  const amount = (key) => (typeof report[key] === 'number' ? report[key] : typeof report[key] === 'string' && report[key].trim() !== '' && Number.isFinite(Number(report[key])) ? Number(report[key]) : null)
  const cell = (key) => (amount(key) === null ? '—' : verified[key] ? money(amount(key)) : `${money(amount(key))} (not on the page)`)
  const delivery = report?.delivery ? (verified.delivery ? 'stated' : 'stated (not on the page)') : '—'
  return [cell('price'), cell('variant_price'), cell('shipping'), delivery]
}

function how(name, way) {
  if (!way) return 'not run'
  if (way.failed && way.turns == null) return `failed: ${way.failed}`
  if (name === 'drive') {
    const tokens = (way.prompt_tokens + way.completion_tokens).toLocaleString('en-US')
    const end = way.refused ? 'refused' : way.stopped === 'reported' ? 'reported' : /^turned away/.test(way.stopped || '') ? 'turned away' : `cut at its ${way.stopped}`
    return `${end}; ${way.turns} turns, ${way.actions} actions, ${tokens} tokens, ${Math.round(way.seconds)} s`
  }
  if (name === 'plain' && way.status == null) return 'no answer'
  if (way.challenge) return way.status ? `HTTP ${way.status}, a bot check` : 'a bot check'
  // The site's own notice that it will not serve this reader. From the plain request fetch_page
  // sends a browser after it; from the render it is the end of the read.
  if (way.refusal) return 'a block notice, not the page'
  if (way.status && way.status !== 200) return `HTTP ${way.status}`
  if (name === 'render' && way.unusable) return 'nothing usable'
  // A result from before fetch_page knew a block notice: it was handed to the model as the page.
  if (way.report?.refused) return 'a refusal page, taken for the page'
  return way.product_price ? 'read, priced by the page’s own data' : 'read'
}

/// Both tables as Markdown. A host that is not public is lettered, from E:
/// the four named hosts stand for A to D.
export function tables(rows, { nameAll = false } = {}) {
  let letter = 'E'.charCodeAt(0)
  const names = rows.map((r) => (r.public || nameAll ? r.host : `shop ${String.fromCharCode(letter++)}`))
  const out = ['| shop | way | price | another variant | delivery to the postcode | when | how it went |', '|---|---|---|---|---|---|---|']
  rows.forEach((r, i) => {
    for (const [name, label] of [['plain', 'plain request'], ['render', 'one-shot render'], ['drive', 'driven session']]) {
      out.push(`| ${name === 'plain' ? names[i] : ''} | ${label} | ${cells(name, r[name]).join(' | ')} | ${how(name, r[name])} |`)
    }
  })
  out.push('', '| shop | gained by the session | read otherwise than the read | a link that picks the variant | a link that fills the basket |', '|---|---|---|---|---|')
  const words = (keys) => keys.map((k) => k.replace('_', ' ')).join(', ')
  rows.forEach((r, i) => {
    const others = [confirmed(r.plain), confirmed(r.render)]
    const g = r.drive?.refused ? [] : gained(confirmed(r.drive), others)
    const d = r.drive?.refused ? [] : differs(confirmed(r.drive), others)
    out.push(`| ${names[i]} | ${g.length ? words(g) : r.drive?.refused ? 'refused' : 'nothing'} | ${d.length ? words(d) : '—'} | ${r.links?.variant || '—'} | ${r.links?.cart || '—'} |`)
  })
  const rule = ruleMet(rows)
  out.push('', `A session read a number the other two ways did not, unrefused, on ${rule.hosts} of ${rows.length} hosts; the rule asks three: ${rule.met ? 'met' : 'not met'}.`)
  return out.join('\n')
}

// ------------------------------------------------------------- the readings

/// An HTTP CONNECT proxy in front of the SOCKS tunnel: Scout's client speaks
/// the first, ssh -D the second, and the handshake with the shop stays the
/// client's own.
function startBridge(socksPort) {
  return new Promise((resolve, reject) => {
    const server = net.createServer((client) => {
      client.on('error', () => {})
      client.once('data', (head) => {
        const m = /^CONNECT ([^:\s]+):(\d+) HTTP\/1\.[01]\r\n/.exec(head.toString('latin1'))
        if (!m) return client.end('HTTP/1.1 405 Method Not Allowed\r\n\r\n')
        const upstream = net.connect(socksPort, '127.0.0.1')
        upstream.on('error', () => client.end('HTTP/1.1 502 Bad Gateway\r\n\r\n'))
        upstream.once('connect', () => {
          upstream.write(Buffer.from([5, 1, 0]))
          upstream.once('data', (hello) => {
            if (hello[0] !== 5 || hello[1] !== 0) return client.end('HTTP/1.1 502 Bad Gateway\r\n\r\n')
            const name = Buffer.from(m[1]), port = Number(m[2])
            upstream.write(Buffer.concat([Buffer.from([5, 1, 0, 3, name.length]), name, Buffer.from([port >> 8, port & 255])]))
            upstream.once('data', (reply) => {
              if (reply[1] !== 0) { upstream.destroy(); return client.end('HTTP/1.1 502 Bad Gateway\r\n\r\n') }
              client.write('HTTP/1.1 200 Connection Established\r\n\r\n')
              upstream.pipe(client)
              client.pipe(upstream)
            })
          })
        })
      })
    })
    server.on('error', reject)
    server.listen(0, '127.0.0.1', () => resolve({ port: server.address().port, close: () => server.close() }))
  })
}

async function extractor(args) {
  const { stdout } = await run(EXTRACTOR, args, { maxBuffer: 64 * 1024 * 1024, timeout: 60_000 })
  return JSON.parse(stdout)
}

/// What a reading comes to, without the page's text.
export function summary(read, extra = {}) {
  const page = read.page || {}
  return {
    status: read.status ?? null, bytes: read.bytes ?? 0, ms: read.ms ?? null, error: read.error ?? null,
    would_render: read.would_render ?? null, challenge: read.challenge ?? false, refusal: read.refusal ?? false,
    product_price: page.product?.price ?? null, struck_through_price: page.product?.struck_through_price ?? null,
    availability: page.availability ?? null,
    text_chars: (page.text || '').length, truncated: page.truncated ?? false, ...extra,
  }
}

function chromeArgs(userAgent, socksPort, profile) {
  // tools/browser.rs, chrome_args — less the two flags only a container needs.
  return [
    '--headless=new', `--user-data-dir=${profile}`, `--user-agent=${userAgent}`,
    `--proxy-server=socks5://127.0.0.1:${socksPort}`, '--proxy-bypass-list=<-loopback>',
    '--disable-gpu', '--disable-extensions', '--no-first-run', '--window-size=1280,1024', '--lang=nl-NL', '--mute-audio',
  ]
}

function renderOnce(url, userAgent, socksPort) {
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'shop-reach-render-'))
  const started = Date.now()
  return new Promise((resolve) => {
    const chrome = spawn(CHROME, [...chromeArgs(userAgent, socksPort, profile), '--dump-dom', `--virtual-time-budget=${VIRTUAL_TIME_BUDGET_MS}`, url], { stdio: ['ignore', 'pipe', 'ignore'] })
    const chunks = []
    let timedOut = false
    const timer = setTimeout(() => { timedOut = true; chrome.kill('SIGKILL') }, RENDER_TIMEOUT_MS)
    chrome.stdout.on('data', (c) => chunks.push(c))
    chrome.on('close', () => {
      clearTimeout(timer)
      fs.rmSync(profile, { recursive: true, force: true })
      resolve({ html: Buffer.concat(chunks).toString('utf8'), ms: Date.now() - started, timedOut })
    })
  })
}

// ---------------------------------------------------------------- the model

const REPORT = {
  type: 'function',
  function: {
    name: 'report_offer',
    description: 'Report what the shop’s pages said. Call it once, when you have what can be had. Leave out anything no page stated.',
    parameters: {
      type: 'object',
      properties: {
        title: { type: 'string', description: 'the product, as the page names it' },
        price: { type: 'number', description: 'the price shown for the product as the page first offered it' },
        variant: { type: 'string', description: 'the other variant you looked at, as the page names it' },
        variant_price: { type: 'number', description: 'the price of that other variant' },
        currency: { type: 'string' },
        shipping: { type: 'number', description: `what delivery to ${POSTCODE}, Netherlands costs for this one product; 0 only if a page says it is free` },
        shipping_note: { type: 'string', description: 'where that was stated and on what condition, in a few words' },
        delivery: { type: 'string', description: 'when it would arrive: only the page’s own words for it, a few words copied exactly, nothing added' },
        refused: { type: 'boolean', description: 'true if the shop showed a bot check, a captcha or an access-denied page' },
        note: { type: 'string', description: 'anything that stopped you, in a sentence' },
      },
    },
  },
}

const fn = (name, description, properties = {}, required = []) => ({ type: 'function', function: { name, description, parameters: { type: 'object', properties, required } } })
const ACTIONS = [
  fn('read', 'The current page again. With more=true, the next part of a long page.', { more: { type: 'boolean' } }),
  fn('click', 'Press the thing numbered n on the current page.', { n: { type: 'integer' } }, ['n']),
  fn('type', 'Type into the field numbered n. Only a postcode, a house number, a quantity or a search term.', { n: { type: 'integer' }, text: { type: 'string' }, enter: { type: 'boolean', description: 'press Enter afterwards' } }, ['n', 'text']),
  fn('select', 'Choose an option in the list numbered n.', { n: { type: 'integer' }, option: { type: 'string' } }, ['n', 'option']),
  fn('back', 'Go back one page.'),
  fn('open', 'Open another address on this same shop.', { url: { type: 'string' } }, ['url']),
]

const ASKED = `Four things, each only as a page of this shop states it:
1. the price of the product as the page first offers it;
2. if the page offers a choice (a size, a colour, a pack), one option other than the one already chosen, and its price;
3. what delivery of this one product to postcode ${POSTCODE} in the Netherlands costs;
4. when it would arrive.
Quote amounts as written. Quote the delivery wording exactly as the page words it. Leave out what no page stated: a guess is worse than a gap.`

const SESSION_BRIEF = `You are reading one shop for a buyer in the Netherlands. You have a browser session on this shop and nowhere else. Each action returns the page as an outline: its title, its address, and its visible text with the things that can be pressed numbered like [7].

${ASKED}

To learn what delivery costs you may put the product in the basket and look at the basket, or read the shop's own page about delivery. Go no further into ordering than that.

Rules that are not yours to bend:
- A cookie banner: choose the option that refuses or allows only what is necessary. Never accept all.
- Never sign in, never create an account, never type a name, an email, a phone number or a street. A postcode (${POSTCODE}), a house number (1), a quantity, a search term: nothing else.
- If the shop shows a bot check, a captcha or an access-denied page, do not try to pass it. Report refused.
- You have twelve turns. When you have what can be had, or cannot get further, call report_offer once.`

const READER_BRIEF = `You are reading one product page for a buyer in the Netherlands. Below is everything a fetch of that page returned: its text, what its markup says about stock, and the product its own data names; a struck_through_price there is what the page gives as its regular or former price, not the price. You cannot open anything else.

${ASKED}

You cannot choose anything here, so report another variant only if the text itself lists another option of this same product with its own price beside it. Another product's price is not a variant's.
"Free above an amount" is free only if this product's price is above that amount; otherwise the text has not said what delivery costs, and shipping is left out.
Set refused only for a bot check or an access-denied page. A text that is nothing but a cookie notice is not that: report nothing and say so in the note.

Call report_offer once.`

function modelSettings(env) {
  return {
    // agent.rs: DEFAULT_BASE_URL, DEFAULT_MODEL
    base: (env.LLM_BASE_URL || env.MINIMAX_BASE_URL || 'https://api.minimax.io/v1').replace(/\/$/, ''),
    key: env.LLM_API_KEY || env.MINIMAX_API_KEY,
    model: env.SHOP_MODEL || env.LLM_MODEL || 'minimax-m3',
  }
}

async function chat(llm, messages, tools) {
  try {
    return await chatOnce(llm, messages, tools)
  } catch (e) {
    // A provider's bad minute is not a finding about a shop.
    await sleep(3000)
    return chatOnce(llm, messages, tools)
  }
}

async function chatOnce(llm, messages, tools) {
  const response = await fetch(`${llm.base}/chat/completions`, {
    method: 'POST',
    headers: { authorization: `Bearer ${llm.key}`, 'content-type': 'application/json' },
    body: JSON.stringify({ model: llm.model, messages, tools, tool_choice: 'auto' }),
    signal: AbortSignal.timeout(90_000),
  })
  if (!response.ok) throw new Error(`the model answered HTTP ${response.status}`)
  const data = await response.json()
  const message = data.choices?.[0]?.message
  if (!message) throw new Error('the model answered with no message')
  return { message: { role: 'assistant', content: message.content ?? '', ...(message.tool_calls?.length ? { tool_calls: message.tool_calls } : {}) }, usage: data.usage || {} }
}

const args = (call) => { try { return JSON.parse(call.function.arguments || '{}') } catch { return {} } }

/// One turn over what fetch_page would have handed the model.
async function readerReport(llm, page) {
  const handed = JSON.stringify({ text: page.text, truncated: page.truncated, availability: page.availability, product: page.product })
  const started = Date.now()
  const { message, usage } = await chat(llm, [{ role: 'system', content: READER_BRIEF }, { role: 'user', content: handed }], [REPORT])
  const call = message.tool_calls?.find((c) => c.function.name === 'report_offer')
  const report = call ? args(call) : null
  const shown = `${page.text} ${page.product?.price ?? ''}`
  return { report, verified: report ? checkReport(report, [shown]) : {}, reader: { prompt_tokens: usage.prompt_tokens ?? 0, completion_tokens: usage.completion_tokens ?? 0, seconds: (Date.now() - started) / 1000 } }
}

// -------------------------------------------------------------- the session

/// Runs in the page. The visible text in reading order, the pressable things
/// numbered, whatever sits in front of the page first.
export function outlineInPage(max, from) {
  const refs = (window.__scoutRefs = {})
  let n = 0
  const parts = []
  const seen = new WeakSet()
  const SKIP = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'IFRAME', 'OBJECT', 'CANVAS', 'VIDEO', 'AUDIO', 'HEAD'])
  const BLOCK = new Set(['DIV', 'P', 'LI', 'UL', 'OL', 'SECTION', 'ARTICLE', 'MAIN', 'HEADER', 'FOOTER', 'NAV', 'ASIDE', 'H1', 'H2', 'H3', 'H4', 'H5', 'H6', 'TR', 'TABLE', 'FORM', 'FIELDSET', 'DL', 'DT', 'DD', 'BR', 'HR', 'DIALOG', 'DETAILS', 'LABEL'])
  const PRESS = 'a[href],button,input:not([type=hidden]),select,textarea,summary,[role=button],[role=link],[role=radio],[role=option],[role=tab],[role=checkbox],[role=menuitem],[role=switch],[role=combobox]'
  const clean = (s) => (s || '').replace(/\s+/g, ' ').trim()
  const shown = (el) => {
    const cs = getComputedStyle(el)
    if (cs.display === 'none' || cs.visibility === 'hidden' || cs.visibility === 'collapse') return false
    return cs.display === 'contents' || el.getClientRects().length > 0
  }
  const labelOf = (el) => {
    const aria = el.getAttribute('aria-label')
    if (aria) return clean(aria)
    if (el.labels && el.labels.length) return clean(el.labels[0].innerText)
    return clean(el.getAttribute('placeholder') || el.getAttribute('title') || el.getAttribute('name') || '')
  }
  const describe = (el) => {
    const tag = el.tagName
    if (tag === 'SELECT') {
      const options = [...el.options].slice(0, 15).map((o) => (o.selected ? '*' : '') + clean(o.textContent)).join(' | ')
      return `[${n}] list "${labelOf(el)}": ${options}`
    }
    if (tag === 'INPUT' || tag === 'TEXTAREA') {
      const type = (el.getAttribute('type') || 'text').toLowerCase()
      if (type === 'checkbox' || type === 'radio') return `[${n}] ${type}${el.checked ? ' (on)' : ''} "${labelOf(el)}"`
      if (type === 'submit' || type === 'button' || type === 'image') return `[${n}] button "${clean(el.value) || labelOf(el)}"`
      return `[${n}] field "${labelOf(el)}"${el.value ? ` = "${clean(el.value).slice(0, 40)}"` : ''}`
    }
    const img = el.querySelector('img[alt]')
    const text = clean(el.innerText).slice(0, 120) || clean(el.getAttribute('aria-label') || el.getAttribute('title') || (img ? img.alt : ''))
    if (!text) return null
    const on = ['aria-pressed', 'aria-selected', 'aria-checked'].some((a) => el.getAttribute(a) === 'true') ? ' (on)' : ''
    const off = el.disabled || el.getAttribute('aria-disabled') === 'true' ? ' (off)' : ''
    return `[${n}] ${tag === 'A' ? 'link' : 'button'}${on}${off} "${text}"`
  }
  const walk = (node) => {
    if (node.nodeType === 3) {
      // As written, spaces and all: "259" and ",-" in two spans are one amount.
      parts.push(node.nodeValue)
      return
    }
    if (node.nodeType !== 1 || seen.has(node)) return
    seen.add(node)
    if (SKIP.has(node.tagName) || node.namespaceURI !== 'http://www.w3.org/1999/xhtml') return
    if (node.getAttribute('aria-hidden') === 'true' || !shown(node)) return
    if (node.matches(PRESS)) {
      n += 1
      const said = describe(node)
      if (said) { refs[n] = node; parts.push(` ${said} `) } else n -= 1
      return
    }
    for (const child of node.shadowRoot ? node.shadowRoot.childNodes : node.childNodes) walk(child)
    if (BLOCK.has(node.tagName)) parts.push('\n')
  }
  const section = (title, roots) => {
    const before = parts.length
    for (const root of roots) walk(root)
    if (parts.slice(before).join('').trim()) parts.splice(before, 0, `\n# ${title}\n`)
  }
  const front = [...document.querySelectorAll('dialog[open],[role=dialog],[role=alertdialog],[aria-modal=true],[id*=cookie i],[class*=cookie i],[id*=consent i],[class*=consent i],[id*=onetrust i]')].filter((e) => shown(e))
  section('In front of the page', front)
  section('The page', [...document.querySelectorAll('main,[role=main]')])
  section('Around it', document.body ? [document.body] : [])
  const text = parts.join('').replace(/[^\S\n]+/g, ' ').replace(/ ?\n ?/g, '\n').replace(/\n{2,}/g, '\n').trim()
  return { title: document.title, url: location.href, text: text.slice(from, from + max), more: text.length > from + max, total: text.length }
}

function targetInPage(n) {
  const el = (window.__scoutRefs || {})[n]
  if (!el || !el.isConnected) return { error: `nothing is numbered ${n} on this page now; read it again` }
  if (el.tagName === 'A' && el.target && el.target !== '_self') el.target = '_self'
  el.scrollIntoView({ block: 'center', inline: 'center' })
  const r = el.getBoundingClientRect()
  const label = el.labels && el.labels.length ? el.labels[0].innerText : ''
  return {
    x: r.left + r.width / 2, y: r.top + r.height / 2, sized: r.width > 0 && r.height > 0,
    href: el.tagName === 'A' ? el.href : null, tag: el.tagName,
    field: { type: el.getAttribute('type'), autocomplete: el.getAttribute('autocomplete'), name: el.getAttribute('name'), id: el.id, placeholder: el.getAttribute('placeholder'), title: el.getAttribute('title'), label: (el.getAttribute('aria-label') || '') + ' ' + label },
  }
}

function focusInPage(n) {
  const el = (window.__scoutRefs || {})[n]
  if (!el) return false
  el.focus()
  if (el.select) el.select()
  return true
}

function pressInPage(n) {
  const el = (window.__scoutRefs || {})[n]
  if (el) el.click()
  return !!el
}

function chooseInPage(n, option) {
  const el = (window.__scoutRefs || {})[n]
  if (!el || !el.isConnected) return { error: `nothing is numbered ${n} on this page now; read it again` }
  if (el.tagName !== 'SELECT') return { error: 'that is not a list; press the option itself' }
  const flat = (s) => s.replace(/\s+/g, ' ').trim().toLowerCase()
  const want = flat(option)
  const found = [...el.options].find((o) => sameOption(o.textContent, option)) || [...el.options].find((o) => want.length > 1 && flat(o.textContent).includes(want))
  if (!found) return { error: `that list has no such option; it has: ${[...el.options].slice(0, 15).map((o) => flat(o.textContent)).join(' | ')}` }
  el.value = found.value
  el.dispatchEvent(new Event('input', { bubbles: true }))
  el.dispatchEvent(new Event('change', { bubbles: true }))
  return { chosen: flat(found.textContent) }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

class Session {
  static async open(userAgent, socksPort) {
    const s = new Session()
    s.profile = fs.mkdtempSync(path.join(os.tmpdir(), 'shop-reach-session-'))
    s.chrome = spawn(CHROME, [...chromeArgs(userAgent, socksPort, s.profile), '--remote-debugging-port=0', 'about:blank'], { stdio: 'ignore' })
    const portFile = path.join(s.profile, 'DevToolsActivePort')
    for (let i = 0; i < 100 && !fs.existsSync(portFile); i++) await sleep(100)
    const port = fs.readFileSync(portFile, 'utf8').split('\n')[0]
    const pages = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()
    s.ws = new WebSocket(pages.find((p) => p.type === 'page').webSocketDebuggerUrl)
    await new Promise((resolve, reject) => { s.ws.addEventListener('open', resolve, { once: true }); s.ws.addEventListener('error', reject, { once: true }) })
    s.id = 0
    s.waiting = new Map()
    s.status = null
    s.ws.addEventListener('message', (event) => {
      const m = JSON.parse(event.data)
      if (m.id && s.waiting.has(m.id)) {
        const { resolve, reject } = s.waiting.get(m.id)
        s.waiting.delete(m.id)
        m.error ? reject(new Error(m.error.message)) : resolve(m.result)
      } else if (m.method === 'Network.responseReceived' && m.params.type === 'Document' && m.params.frameId === s.frame) {
        s.status = m.params.response.status
      } else if (m.method === 'Page.javascriptDialogOpening') {
        // An alert would hold every later call; nobody is here to read it.
        // Declined, except the one that asks before leaving, which would keep the session there.
        s.send('Page.handleJavaScriptDialog', { accept: m.params.type === 'beforeunload' }).catch(() => {})
      }
    })
    await s.send('Page.enable')
    await s.send('Network.enable')
    s.frame = (await s.send('Page.getFrameTree')).frameTree.frame.id
    // No side doors: nothing is downloaded.
    await s.send('Browser.setDownloadBehavior', { behavior: 'deny' }).catch(() => {})
    return s
  }

  send(method, params = {}) {
    return new Promise((resolve, reject) => {
      const id = ++this.id
      const timer = setTimeout(() => { this.waiting.delete(id); reject(new Error(`${method} did not answer`)) }, 30_000)
      this.waiting.set(id, { resolve: (v) => { clearTimeout(timer); resolve(v) }, reject: (e) => { clearTimeout(timer); reject(e) } })
      this.ws.send(JSON.stringify({ id, method, params }))
    })
  }

  async evaluate(func, ...values) {
    // sameOption rides along: a function in the page sees nothing of this file.
    const expression = `(() => { ${sameOption.toString()}; return (${func.toString()})(${values.map((v) => JSON.stringify(v)).join(',')}) })()`
    const { result, exceptionDetails } = await this.send('Runtime.evaluate', { expression, returnByValue: true })
    if (exceptionDetails) throw new Error(exceptionDetails.exception?.description || exceptionDetails.text)
    return result.value
  }

  /// Until the page has stopped loading, and a moment more for what scripts add.
  async settle() {
    await sleep(400)
    for (let i = 0; i < 48; i++) {
      const state = await this.send('Runtime.evaluate', { expression: 'document.readyState', returnByValue: true }).then((r) => r.result.value, () => 'loading')
      if (state === 'complete') break
      await sleep(250)
    }
    await sleep(900)
  }

  async navigate(url) {
    this.status = null
    await this.send('Page.navigate', { url })
    await this.settle()
  }

  async outline(from = 0) {
    for (let attempt = 0; ; attempt++) {
      try {
        return await this.evaluate(outlineInPage, OUTLINE_CHARS, from)
      } catch (e) {
        if (attempt >= 2) return { title: '', url: '', text: `(the page could not be read: ${e.message})`, more: false, total: 0 }
        await sleep(700)  // a page still swapping itself out
      }
    }
  }

  async click(n) {
    const target = await this.evaluate(targetInPage, n)
    if (target.error) return target
    if (target.href && /^https?:/.test(target.href) && !sameSite(target.href, this.home)) return { error: 'refused: that link leaves this shop' }
    if (!target.sized) await this.evaluate(pressInPage, n)
    else {
      const at = { x: target.x, y: target.y, button: 'left', clickCount: 1 }
      await this.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: target.x, y: target.y })
      await this.send('Input.dispatchMouseEvent', { type: 'mousePressed', ...at })
      await this.send('Input.dispatchMouseEvent', { type: 'mouseReleased', ...at })
    }
    return {}
  }

  async type(n, text, enter) {
    const target = await this.evaluate(targetInPage, n)
    if (target.error) return target
    if (!['INPUT', 'TEXTAREA'].includes(target.tag)) return { error: 'that is not a field' }
    const why = typingRefused(target.field, text)
    if (why) return { error: `refused: ${why}` }
    await this.evaluate(focusInPage, n)
    await this.send('Input.insertText', { text: String(text) })
    if (enter) {
      const key = { key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13, nativeVirtualKeyCode: 13, text: '\r' }
      await this.send('Input.dispatchKeyEvent', { type: 'keyDown', ...key })
      await this.send('Input.dispatchKeyEvent', { type: 'keyUp', ...key })
    }
    return {}
  }

  /// One private profile, gone when the session closes: cookies, history and all.
  async close() {
    try { this.ws.close() } catch {}
    const gone = new Promise((resolve) => this.chrome.once('exit', resolve))
    this.chrome.kill('SIGKILL')
    await Promise.race([gone, sleep(3000)])
    fs.rmSync(this.profile, { recursive: true, force: true, maxRetries: 3, retryDelay: 200 })
  }
}

/// One driven session on one product page.
async function drive(llm, url, userAgent, socksPort, limits, audit) {
  const started = Date.now()
  const elapsed = () => (Date.now() - started) / 1000
  const session = await Session.open(userAgent, socksPort)
  session.home = url
  const shown = []       // every outline's text, for checking the report
  const log = []
  const out = { refused: false, first_wall: false, model_refused: false, walls: 0, stopped: null, turns: 0, actions: 0, prompt_tokens: 0, completion_tokens: 0, seconds: 0, report: null, verified: {}, verified_last: {}, first_status: null, final_url: null, log }
  let pages = 0, lastGood = url, from = 0, wallsInARow = 0

  const page = async (note, more = false) => {
    from = more ? from + OUTLINE_CHARS : 0
    let o = await session.outline(from)
    const notes = note ? [note] : []
    if (o.url && /^https?:/.test(o.url) && !sameSite(o.url, url)) {
      notes.push('Refused: that led away from this shop, so the session went back.')
      await session.navigate(lastGood)
      o = await session.outline(0)
    } else if (o.url) lastGood = o.url
    const wall = refusedBy(session.status, o.title, o.text)
    wallsInARow = wall ? wallsInARow + 1 : 0
    if (wall) {
      out.walls += 1
      if (pages === 0) out.first_wall = true
      notes.push(`This is a refusal or a bot check${session.status ? ` (HTTP ${session.status})` : ''}. Do not try to pass it; report refused.`)
    }
    shown.push(o.text)
    pages += 1
    audit(`\n===== PAGE ${pages} ${o.url} [status ${session.status}] =====\n${notes.join('\n')}\n${o.text}\n`)
    return [`PAGE ${pages}`, o.title, o.url, ...notes, '---', o.text, o.more ? '(the page goes on: read with more=true)' : ''].join('\n')
  }

  try {
    await session.navigate(url)
    out.first_status = session.status
    const first = await page('')
    // The first page arrives as the answer to a read, so that it is trimmed
    // from the conversation like any other once two newer ones exist.
    const history = [
      { role: 'system', content: SESSION_BRIEF },
      { role: 'user', content: `The product page is open. Its address: ${url}` },
      { role: 'assistant', content: '', tool_calls: [{ id: 'first', type: 'function', function: { name: 'read', arguments: '{}' } }] },
      { role: 'tool', tool_call_id: 'first', content: first },
    ]

    for (;;) {
      const away = turnedAway({ firstWall: out.first_wall, walls: out.walls, wallsInARow })
      if (away) { out.stopped = `turned away: ${away}`; break }
      const spent = out.turns >= limits.turns ? 'turns' : out.actions >= limits.actions ? 'actions' : elapsed() >= limits.seconds ? 'seconds' : null
      const last = spent || out.turns === limits.turns - 1
      if (last) history.push({ role: 'user', content: 'That is the budget. Call report_offer now with what the pages have said so far.' })
      const { message, usage } = await chat(llm, trimHistory(history, 2), last ? [REPORT] : [...ACTIONS, REPORT])
      out.turns += 1
      out.prompt_tokens += usage.prompt_tokens ?? 0
      out.completion_tokens += usage.completion_tokens ?? 0
      history.push(message)
      const calls = message.tool_calls || []
      const report = calls.find((c) => c.function.name === 'report_offer')
      if (report) {
        out.report = args(report)
        out.stopped = spent || 'reported'
        break
      }
      if (spent || last) { out.stopped = spent || 'turns'; break }
      if (!calls.length) {
        history.push({ role: 'user', content: 'Act with a tool, or call report_offer.' })
        continue
      }
      for (const call of calls) {
        const a = args(call)
        const name = call.function.name
        let result = {}
        if (out.actions >= limits.actions) {
          history.push({ role: 'tool', tool_call_id: call.id, content: 'The session has no actions left.' })
          continue
        }
        out.actions += 1
        if (name === 'click') result = await session.click(Number(a.n))
        else if (name === 'type') result = await session.type(Number(a.n), String(a.text ?? ''), !!a.enter)
        else if (name === 'select') result = await session.evaluate(chooseInPage, Number(a.n), String(a.option ?? ''))
        else if (name === 'back') await session.evaluate(() => history.back())
        else if (name === 'open') {
          if (sameSite(String(a.url), url)) await session.navigate(String(a.url))
          else result = { error: 'refused: that address is not on this shop' }
        } else if (name !== 'read') result = { error: `there is no tool called ${name}` }
        if (!result.error && name !== 'read' && name !== 'open') await session.settle()
        // What was typed is a postcode or a search term by the rule above; it is logged whole.
        log.push({ turn: out.turns, tool: name, n: a.n ?? null, more: a.more ?? null, text: a.text ?? a.option ?? null, error: result.error ?? null, seconds: Math.round(elapsed()) })
        history.push({ role: 'tool', tool_call_id: call.id, content: await page(result.error ? `That did nothing: ${result.error}` : result.chosen ? `Chosen: ${result.chosen}` : '', name === 'read' && !!a.more) })
      }
    }
    out.final_url = lastGood
    // Refused is the product page refused. A side page that would not open is counted in `walls`.
    out.model_refused = !!out.report?.refused
    out.refused = out.first_wall
    out.verified = checkReport(out.report, shown)
    out.verified_last = checkReport(out.report, shown.slice(-1))
  } catch (e) {
    out.stopped = out.stopped || `error: ${e.message}`
  } finally {
    out.seconds = elapsed()
    await session.close()
  }
  return out
}

// ---------------------------------------------------------------- commands

function flag(argv, name, fallback = null) {
  const i = argv.indexOf(name)
  return i >= 0 ? argv[i + 1] : fallback
}

async function read(argv) {
  const [listFile, outDir] = argv
  if (!listFile || !outDir) throw new Error('usage: shop-reach.mjs read <products.json> <out-dir> [--only label] [--ways plain,render,drive]')
  if (!fs.existsSync(EXTRACTOR)) throw new Error('build the extractor first: cargo build -p scout-core --example page_as_read')
  const socks = Number(flag(argv, '--socks', '18080'))
  const ways = flag(argv, '--ways', 'plain,render,drive').split(',')
  const only = flag(argv, '--only')
  const limits = { turns: Number(flag(argv, '--turns', '12')), actions: Number(flag(argv, '--actions', '25')), seconds: Number(flag(argv, '--seconds', '120')) }
  const llm = modelSettings({ ...parseEnv(fs.readFileSync(path.join(ROOT, '.env'), 'utf8')), ...process.env })
  if (!llm.key) throw new Error('no model key in .env (LLM_API_KEY or MINIMAX_API_KEY)')
  const userAgent = (await extractor(['headers']))['User-Agent']
  fs.mkdirSync(outDir, { recursive: true })
  const bridge = await startBridge(socks)
  try {
    for (const product of JSON.parse(fs.readFileSync(listFile, 'utf8'))) {
      if (only && product.label !== only) continue
      const file = path.join(outDir, `${product.label}.json`)
      const result = fs.existsSync(file) ? JSON.parse(fs.readFileSync(file, 'utf8')) : {}
      Object.assign(result, { label: product.label, host: product.host, public: !!product.public, url: product.url, model: llm.model, at: new Date().toISOString(), limits })
      const say = (what) => console.log(`${product.label}: ${what}`)
      const attempt = async (way, work) => {
        try { await work() } catch (e) { result[way] = { ...(result[way] || {}), failed: e.message }; say(`${way} failed: ${e.message}`) }
        fs.writeFileSync(file, JSON.stringify(result, null, 2))
      }
      if (ways.includes('plain')) await attempt('plain', async () => {
        const html = path.join(outDir, `${product.label}.plain.html`)
        const got = await extractor(['get', product.url, '--proxy', `http://127.0.0.1:${bridge.port}`, '--save', html])
        result.plain = summary(got)
        const readable = got.status === 200 && !got.challenge && !got.refusal && (got.page?.text || '').length > 0
        Object.assign(result.plain, readable ? await readerReport(llm, got.page) : { report: null, verified: {} })
        say(`plain HTTP ${got.status ?? 'none'}, ${got.bytes} bytes, would render ${got.would_render}, priced ${!!result.plain.product_price}`)
      })
      if (ways.includes('render')) await attempt('render', async () => {
        const html = path.join(outDir, `${product.label}.render.html`)
        // A render that hangs is tried once more: the tunnel adds a hop the node does not have.
        let rendered = await renderOnce(product.url, userAgent, socks)
        const retried = rendered.timedOut
        if (retried) rendered = await renderOnce(product.url, userAgent, socks)
        fs.writeFileSync(html, rendered.html)
        const got = await extractor(['file', html, product.url])
        // fetch_page takes a render only when it is not itself a shell, a check or a block notice.
        const unusable = rendered.timedOut || got.unrendered === true || got.refusal === true
        result.render = summary({ ...got, status: unusable ? null : 200, ms: rendered.ms }, { unusable, timed_out: rendered.timedOut, retried })
        Object.assign(result.render, !unusable && (got.page?.text || '').length > 0 ? await readerReport(llm, got.page) : { report: null, verified: {} })
        say(`render ${rendered.ms} ms, ${got.bytes} bytes, usable ${!unusable}, priced ${!!result.render.product_price}`)
      })
      if (ways.includes('drive')) await attempt('drive', async () => {
        const auditFile = path.join(outDir, `${product.label}.session.txt`)
        fs.writeFileSync(auditFile, '')
        result.drive = await drive(llm, product.url, userAgent, socks, limits, (text) => fs.appendFileSync(auditFile, text))
        const d = result.drive
        say(`drive ${d.stopped}, refused ${d.refused}, ${d.turns} turns, ${d.actions} actions, ${d.prompt_tokens + d.completion_tokens} tokens, ${Math.round(d.seconds)} s`)
      })
    }
  } finally {
    bridge.close()
  }
}

function table(argv) {
  const [outDir] = argv
  if (!outDir) throw new Error('usage: shop-reach.mjs table <out-dir> [--name-all]')
  const linksFile = path.join(outDir, 'links.json')
  const links = fs.existsSync(linksFile) ? JSON.parse(fs.readFileSync(linksFile, 'utf8')) : {}
  const orderFile = path.join(outDir, 'order.json')
  const order = fs.existsSync(orderFile) ? JSON.parse(fs.readFileSync(orderFile, 'utf8')) : null
  let rows = fs.readdirSync(outDir).filter((f) => f.endsWith('.json') && !['links.json', 'order.json'].includes(f)).map((f) => JSON.parse(fs.readFileSync(path.join(outDir, f), 'utf8')))
  rows = rows.map((r) => ({ ...r, links: links[r.label] }))
  if (order) rows.sort((a, b) => order.indexOf(a.label) - order.indexOf(b.label))
  console.log(tables(rows, { nameAll: argv.includes('--name-all') }))
}

/// The checks again, over the pages already saved: a session's outlines
/// from its record, a read's text from the extractor. No shop is asked anything.
async function recheck(argv) {
  const [outDir] = argv
  if (!outDir) throw new Error('usage: shop-reach.mjs recheck <out-dir>')
  for (const name of fs.readdirSync(outDir).filter((f) => f.endsWith('.json') && !['links.json', 'order.json'].includes(f))) {
    const file = path.join(outDir, name)
    const result = JSON.parse(fs.readFileSync(file, 'utf8'))
    for (const way of ['plain', 'render']) {
      const html = path.join(outDir, `${result.label}.${way}.html`)
      if (!result[way]?.report || !fs.existsSync(html)) continue
      const { page } = await extractor(['file', html, result.url])
      result[way].verified = checkReport(result[way].report, [`${page.text} ${page.product?.price ?? ''}`])
    }
    const record = path.join(outDir, `${result.label}.session.txt`)
    if (result.drive?.report && fs.existsSync(record)) {
      const pages = fs.readFileSync(record, 'utf8').split(/\n===== PAGE \d+ [^\n]*=====\n/).slice(1)
      result.drive.verified = checkReport(result.drive.report, pages)
      result.drive.verified_last = checkReport(result.drive.report, pages.slice(-1))
    }
    fs.writeFileSync(file, JSON.stringify(result, null, 2))
    console.log(`${result.label}: rechecked`)
  }
}

async function follow(argv) {
  const needles = argv.flatMap((a, i) => (a === '--needle' ? [argv[i + 1]] : []))
  const addresses = argv.filter((a, i) => /^https?:/.test(a) && argv[i - 1] !== '--needle')
  if (!addresses.length) throw new Error('usage: shop-reach.mjs follow <address>... [--needle text]...')
  const session = await Session.open((await extractor(['headers']))['User-Agent'], Number(flag(argv, '--socks', '18080')))
  try {
    for (const address of addresses) {
      await session.navigate(address)
      const o = await session.outline(0)
      const whole = o.more ? (await session.evaluate(outlineInPage, 200_000, 0)).text : o.text
      const where = (u) => { try { const x = new URL(u); return x.pathname + x.search.slice(0, 40) } catch { return u } }
      console.log(JSON.stringify({ asked: where(address), landed: where(o.url), status: session.status, title: o.title.slice(0, 50), chars: whole.length, wrote: Object.fromEntries(needles.map((n) => [n, whole.toLowerCase().includes(n.toLowerCase())])) }))
    }
  } finally {
    await session.close()
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [command, ...rest] = process.argv.slice(2)
  const commands = { read, table, follow, recheck }
  if (!commands[command]) {
    console.error('usage: shop-reach.mjs read <products.json> <out-dir> | table <out-dir> | recheck <out-dir> | follow <address>...')
    process.exit(2)
  }
  Promise.resolve(commands[command](rest)).then(() => process.exit(0), (e) => { console.error(e.message); process.exit(1) })
}
