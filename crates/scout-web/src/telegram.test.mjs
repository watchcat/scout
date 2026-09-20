import { test } from 'node:test'
import assert from 'node:assert/strict'
import {
  launchData, tripPageUrl, justExchanged, markExchanged, clearExchanged,
  telegramEvent, openOutside, LAUNCH_KEY, LOOP_WINDOW_MS,
} from './telegram.js'
import * as telegramExports from './telegram.js'

function memory() {
  const map = new Map()
  return {
    getItem: (k) => (map.has(k) ? map.get(k) : null),
    setItem: (k, v) => map.set(k, String(v)),
    removeItem: (k) => map.delete(k),
  }
}

const broken = {
  getItem() { throw new Error('denied') },
  setItem() { throw new Error('denied') },
  removeItem() { throw new Error('denied') },
}

test('the launch data comes from the fragment and is kept for a reload', () => {
  const store = memory()
  const signed = 'auth_date=1&user=%7B%22id%22%3A7%7D&hash=ab'
  const hash = `#tgWebAppData=${encodeURIComponent(signed)}&tgWebAppVersion=8.0&tgWebAppPlatform=ios`
  assert.equal(launchData(hash, store), signed)
  assert.equal(store.getItem(LAUNCH_KEY), signed)
  // The reload: the fragment is gone, the tab still has it.
  assert.equal(launchData('', store), signed)
})

test('no launch data is null, and a storage that throws does not stop a launch', () => {
  assert.equal(launchData('', memory()), null)
  assert.equal(launchData('#tgWebAppVersion=8.0', memory()), null)
  assert.equal(launchData('#tgWebAppData=x%3D1', broken), 'x=1')
  assert.equal(launchData('', broken), null)
  assert.equal(launchData('', undefined), null)
})

test('the trip page is asked for in its Telegram shape, on the named trip', () => {
  assert.equal(tripPageUrl(''), '/chat?in=telegram')
  assert.equal(tripPageUrl('?trip=Hong+Kong'), '/chat?in=telegram&trip=Hong+Kong')
  // Only the trip travels: nothing else in the launch URL is passed on.
  assert.equal(tripPageUrl('?trip=Lisbon&in=web&x=1'), '/chat?in=telegram&trip=Lisbon')
})

test('a bounce straight back to the launch page is told apart from a new launch', () => {
  const store = memory()
  assert.equal(justExchanged(store, 1_000_000), false)
  markExchanged(store, 1_000_000)
  assert.equal(justExchanged(store, 1_000_000 + 800), true)
  assert.equal(justExchanged(store, 1_000_000 + LOOP_WINDOW_MS + 1), false)
  markExchanged(store, 2_000_000)
  clearExchanged(store)
  assert.equal(justExchanged(store, 2_000_000 + 1), false)
  assert.equal(justExchanged(broken, 5), false)
})

test('an event goes to whichever bridge the client put there, and nowhere else', () => {
  const sent = []
  const phone = { TelegramWebviewProxy: { postEvent: (t, d) => sent.push(['proxy', t, d]) } }
  phone.parent = phone
  assert.equal(telegramEvent('web_app_open_link', { url: 'https://x' }, phone), true)
  assert.deepEqual(sent.pop(), ['proxy', 'web_app_open_link', '{"url":"https://x"}'])

  const framed = { parent: { postMessage: (m, o) => sent.push(['frame', m, o]) } }
  assert.equal(telegramEvent('web_app_ready', {}, framed), true)
  assert.deepEqual(sent.pop(), ['frame', '{"eventType":"web_app_ready","eventData":{}}', 'https://web.telegram.org'])

  // A plain browser tab: nothing to talk to, and nothing pretended.
  const tab = {}
  tab.parent = tab
  assert.equal(telegramEvent('web_app_ready', {}, tab), false)
})

test('a link outside Telegram asks the client, or falls back to a tab', () => {
  const sent = []
  const phone = { TelegramWebviewProxy: { postEvent: (t, d) => sent.push([t, JSON.parse(d)]) } }
  phone.parent = phone
  openOutside('https://goodscout.fyi/tg/file?t=abc', phone)
  assert.deepEqual(sent, [['web_app_open_link', { url: 'https://goodscout.fyi/tg/file?t=abc' }]])

  const opened = []
  const tab = { open: (...args) => opened.push(args) }
  tab.parent = tab
  openOutside('https://example.com', tab)
  assert.deepEqual(opened, [['https://example.com', '_blank', 'noopener,noreferrer']])
})

test('the settings button is asked for, and a press is heard from either bridge and no other origin', () => {
  const { listenToTelegram, showSettingsButton } = telegramExports
  const sent = []
  const heard = []
  const listeners = {}
  const phone = {
    TelegramWebviewProxy: { postEvent: (t, d) => sent.push([t, JSON.parse(d)]) },
    addEventListener: (name, fn) => { listeners[name] = fn },
  }
  phone.parent = phone
  assert.equal(showSettingsButton(phone), true)
  assert.deepEqual(sent, [['web_app_setup_settings_button', { is_visible: true }]])
  listenToTelegram((t, d) => heard.push([t, d]), phone)
  // The native bridge.
  phone.Telegram.WebView.receiveEvent('settings_button_pressed', {})
  // Telegram Web, and an impostor that framed us.
  listeners.message({ origin: 'https://web.telegram.org', data: JSON.stringify({ eventType: 'settings_button_pressed', eventData: {} }) })
  listeners.message({ origin: 'https://evil.example', data: JSON.stringify({ eventType: 'settings_button_pressed', eventData: {} }) })
  listeners.message({ origin: 'https://web.telegram.org', data: 'not json' })
  assert.deepEqual(heard, [['settings_button_pressed', {}], ['settings_button_pressed', {}]])
})
