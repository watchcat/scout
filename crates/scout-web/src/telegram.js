// What the trip page needs to know about running inside Telegram, shared by
// the launch page (`tg.js`) and the page it hands over to (`chat.js`).
//
// Telegram's own `telegram-web-app.js` would do all of this and a great
// deal more, but it is a third party's script running in the page that
// holds the session and the form token. The part of it this page uses is
// a few event names and where to send them, and those are written down
// in Telegram's documentation — so that is all there is here.

// Where the launch data is kept for the life of the tab. Telegram hands it
// over once, in the fragment of the first URL; a reload — or a cookie that
// went missing mid-visit — has only this to sign in again with.
export const LAUNCH_KEY = 'scout-telegram-launch'
// When the launch page last traded launch data for a session. See
// `justExchanged`.
export const EXCHANGED_KEY = 'scout-telegram-exchanged'
// How recently counts as "just". A page that works comes back from the
// exchange in well under a second; a loop comes back as fast as it can.
export const LOOP_WINDOW_MS = 10_000

// The raw `initData` Telegram signed: from the fragment when this is the
// launch, from the tab's storage when it is not. Storage can be missing or
// can throw, and neither may stop a launch that has the fragment.
export function launchData(hash, storage) {
  const fresh = new URLSearchParams(String(hash || '').replace(/^#/, '')).get('tgWebAppData')
  if (fresh) {
    try { storage?.setItem(LAUNCH_KEY, fresh) } catch { /* the fragment is enough */ }
    return fresh
  }
  try { return storage?.getItem(LAUNCH_KEY) || null } catch { return null }
}

// Where the launch page sends the reader once signed in: the trip page in
// its Telegram shape, on the trip a button named, if one did.
export function tripPageUrl(search) {
  const trip = new URLSearchParams(String(search || '')).get('trip')
  const next = new URLSearchParams({ in: 'telegram' })
  if (trip) next.set('trip', trip)
  return `/chat?${next}`
}

// Whether the launch page is being shown again straight after it signed
// in. The trip page sends a reader with no session back to it, so a
// browser that will not keep the cookie — Safari inside Telegram Web
// refuses a framed site's cookies outright — would otherwise bounce the
// two pages off each other forever.
export function justExchanged(storage, now) {
  try {
    const at = Number(storage?.getItem(EXCHANGED_KEY))
    return Number.isFinite(at) && at > 0 && now - at < LOOP_WINDOW_MS
  } catch {
    return false
  }
}

export function markExchanged(storage, now) {
  try { storage?.setItem(EXCHANGED_KEY, String(now)) } catch { /* no loop guard, then */ }
}

// Called by the trip page once it is up: it did keep the session, so the
// next launch page is a new launch and not a bounce.
export function clearExchanged(storage) {
  try { storage?.removeItem(EXCHANGED_KEY) } catch { /* nothing to clear */ }
}

// Sends one event to the Telegram client around this page, and says
// whether there was one to send it to. The three routes are the ones
// Telegram documents: a native bridge in the phone and desktop apps, the
// Windows Phone one, and the parent frame in Telegram Web. The frame is
// named rather than `'*'`, so an event never goes to a page that framed
// us without being Telegram.
export function telegramEvent(eventType, eventData = {}, win = globalThis.window) {
  if (!win) return false
  const body = JSON.stringify({ eventType, eventData })
  if (win.TelegramWebviewProxy?.postEvent) {
    win.TelegramWebviewProxy.postEvent(eventType, JSON.stringify(eventData))
    return true
  }
  if (win.external && typeof win.external.notify === 'function') {
    win.external.notify(body)
    return true
  }
  if (win.parent && win.parent !== win) {
    win.parent.postMessage(body, 'https://web.telegram.org')
    return true
  }
  return false
}

// Opens a page outside Telegram's web view. A link followed inside it
// either replaces the trip page or is refused, depending on the app;
// asking the client to open it is the one behaviour they all share.
export function openOutside(url, win = globalThis.window) {
  if (telegramEvent('web_app_open_link', { url }, win)) return
  win?.open(url, '_blank', 'noopener,noreferrer')
}

// The page's own ground, so the strip Telegram draws above it is the same
// colour rather than the client's theme cutting across the top.
export const TELEGRAM_GROUND = '#002b36'

// Asks the client for the "Settings" entry in the ⋯ menu it draws over
// the page. Telegram names the entry; the page only gets told it was
// pressed, through `listenToTelegram`.
export function showSettingsButton(win = globalThis.window) {
  return telegramEvent('web_app_setup_settings_button', { is_visible: true }, win)
}

// Hears what the client sends back. The phone and desktop apps call
// `window.Telegram.WebView.receiveEvent`; Telegram Web posts a message
// from its own origin, and only that origin is listened to — a page that
// framed us and is not Telegram gets to say nothing.
export function listenToTelegram(handler, win = globalThis.window) {
  if (!win) return
  win.Telegram = { WebView: { receiveEvent: (eventType, eventData) => handler(eventType, eventData) } }
  win.addEventListener?.('message', (event) => {
    if (event.origin !== 'https://web.telegram.org') return
    let body
    try { body = JSON.parse(event.data) } catch { return }
    if (body && typeof body.eventType === 'string') handler(body.eventType, body.eventData)
  })
}

export function settleIntoTelegram(win = globalThis.window) {
  telegramEvent('web_app_ready', {}, win)
  telegramEvent('web_app_expand', {}, win)
  telegramEvent('web_app_set_header_color', { color: TELEGRAM_GROUND }, win)
  telegramEvent('web_app_set_background_color', { color: TELEGRAM_GROUND }, win)
}
