import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import vm from 'node:vm'
import { telegramTheme, TELEGRAM_SCHEME_KEY } from './telegram.js'

// `theme.js` is a classic script that runs as the page loads, so it is run
// the same way here: in a fresh context per case, with the parts of a page
// it touches stood in for.
const SCRIPT = readFileSync(new URL('./theme.js', import.meta.url), 'utf8')

function page({ stored = null, light = false, storage = 'works', hash = '', surface, session = {} } = {}) {
  const saved = new Map(stored === null ? [] : [['scout-theme', stored]])
  const denied = () => { throw new Error('storage is blocked') }
  const localStorage = storage === 'works'
    ? {
        getItem: (k) => (saved.has(k) ? saved.get(k) : null),
        setItem: (k, v) => saved.set(k, String(v)),
        removeItem: (k) => saved.delete(k),
      }
    : { getItem: denied, setItem: denied, removeItem: denied }
  const media = { matches: light, listeners: [], addEventListener(_, fn) { this.listeners.push(fn) } }
  const select = { value: '', listeners: [], addEventListener(_, fn) { this.listeners.push(fn) } }
  const loaded = []
  const root = { dataset: surface ? { surface } : {} }
  const kept = new Map(Object.entries(session))
  const sessionStorage = {
    getItem: (k) => (kept.has(k) ? kept.get(k) : null),
    setItem: (k, v) => kept.set(k, String(v)),
    removeItem: (k) => kept.delete(k),
  }
  const document = {
    documentElement: root,
    getElementById: (id) => (id === 'theme-select' ? select : null),
    addEventListener: (name, fn) => { if (name === 'DOMContentLoaded') loaded.push(fn) },
  }
  const window = { matchMedia: (q) => (q === '(prefers-color-scheme: light)' ? media : null) }
  vm.runInNewContext(SCRIPT, { window, document, localStorage, sessionStorage, location: { hash }, URLSearchParams })
  for (const fn of loaded) fn()
  return {
    theme: () => root.dataset.theme,
    saved: () => (saved.has('scout-theme') ? saved.get('scout-theme') : null),
    kept: (k) => (kept.has(k) ? kept.get(k) : null),
    select,
    pick(value) {
      select.value = value
      for (const fn of select.listeners) fn()
    },
    osTurns(isLight) {
      media.matches = isLight
      for (const fn of media.listeners) fn()
    },
  }
}

test('with nothing chosen the page follows the system, and says so', () => {
  assert.equal(page({ light: false }).theme(), 'solarized-dark')
  const day = page({ light: true })
  assert.equal(day.theme(), 'solarized-light')
  assert.equal(day.select.value, 'system')
  // An evening switch to dark mode reaches an open page.
  day.osTurns(false)
  assert.equal(day.theme(), 'solarized-dark')
})

test('a chosen theme wins over the system, and survives the system changing', () => {
  const p = page({ stored: 'light', light: false })
  assert.equal(p.theme(), 'light')
  assert.equal(p.select.value, 'light')
  p.osTurns(true)
  assert.equal(p.theme(), 'light')
})

test('picking a theme applies and keeps it; picking System forgets the choice', () => {
  const p = page({ light: true })
  p.pick('black')
  assert.deepEqual([p.theme(), p.saved()], ['black', 'black'])
  p.pick('system')
  assert.deepEqual([p.theme(), p.saved()], ['solarized-light', null])
  p.pick('neon')
  assert.equal(p.theme(), 'solarized-light', 'a value that is not a theme changes nothing')
})

test('a stored value that is not a theme is the system, and blocked storage still lets a reader pick', () => {
  assert.equal(page({ stored: 'neon', light: true }).theme(), 'solarized-light')
  const p = page({ storage: 'blocked', light: false })
  assert.equal(p.theme(), 'solarized-dark')
  p.pick('light')
  assert.equal(p.theme(), 'light')
})

// Inside Telegram, Telegram's theme decides. The launch page reads it off
// the address Telegram opened it with and keeps it for the page it opens,
// which no longer has that address.
const launchedWith = (bg) => '#tgWebAppData=x&tgWebAppThemeParams=' + encodeURIComponent(JSON.stringify({ bg_color: bg }))

test('the launch page takes Telegram\'s theme over the device and over a website choice', () => {
  const day = page({ hash: launchedWith('#ffffff'), light: false, stored: 'black' })
  assert.equal(day.theme(), 'solarized-light')
  assert.equal(day.kept(TELEGRAM_SCHEME_KEY), 'solarized-light')
  const night = page({ hash: launchedWith('#17212b'), light: true })
  assert.equal(night.theme(), 'solarized-dark')
})

test('the trip page inside Telegram keeps the theme the launch page found', () => {
  const p = page({ surface: 'telegram', session: { [TELEGRAM_SCHEME_KEY]: 'solarized-light' }, light: false })
  assert.equal(p.theme(), 'solarized-light')
  // Not a scheme this script would have written: the device decides.
  assert.equal(page({ surface: 'telegram', session: { [TELEGRAM_SCHEME_KEY]: 'neon' }, light: true }).theme(), 'solarized-light')
  // Opened without a launch: the device decides, as on the website.
  assert.equal(page({ surface: 'telegram', light: false }).theme(), 'solarized-dark')
  // The website, with something kept from an earlier Mini App: ignored.
  assert.equal(page({ session: { [TELEGRAM_SCHEME_KEY]: 'solarized-light' }, light: false }).theme(), 'solarized-dark')
})

test('theme.js and telegram.js read Telegram\'s colours the same way', () => {
  // Two copies of one rule — the classic script cannot import — so they
  // are held to each other here.
  for (const bg of ['#ffffff', '#f4f4f5', '#17212b', '#212121', '#000000', '#7f7f7f', '#808080', '#3e546a']) {
    assert.equal(page({ hash: launchedWith(bg) }).theme(), telegramTheme(bg), bg)
  }
})
