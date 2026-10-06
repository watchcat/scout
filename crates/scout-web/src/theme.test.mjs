import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import vm from 'node:vm'

// `theme.js` is a classic script that runs as the page loads, so it is run
// the same way here: in a fresh context per case, with the parts of a page
// it touches stood in for.
const SCRIPT = readFileSync(new URL('./theme.js', import.meta.url), 'utf8')

function page({ stored = null, light = false, storage = 'works' } = {}) {
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
  const root = { dataset: {} }
  const document = {
    documentElement: root,
    getElementById: (id) => (id === 'theme-select' ? select : null),
    addEventListener: (name, fn) => { if (name === 'DOMContentLoaded') loaded.push(fn) },
  }
  const window = { matchMedia: (q) => (q === '(prefers-color-scheme: light)' ? media : null) }
  vm.runInNewContext(SCRIPT, { window, document, localStorage })
  for (const fn of loaded) fn()
  return {
    theme: () => root.dataset.theme,
    saved: () => (saved.has('scout-theme') ? saved.get('scout-theme') : null),
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
