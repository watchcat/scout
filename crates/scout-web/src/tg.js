// The Mini App's front door: trade the launch data Telegram signed for a
// session, then hand over to the trip page. Everything this decides is in
// `telegram.js`, where the tests are; this file is the order of events.
import {
  launchData, tripPageUrl, justExchanged, markExchanged, settleIntoTelegram,
} from './telegram.js'

const launch = document.getElementById('launch')
const title = document.getElementById('launch-title')
const copy = document.getElementById('launch-copy')

function say(heading, sentence) {
  launch.setAttribute('aria-busy', 'false')
  title.textContent = heading
  copy.textContent = sentence
}

function storage() {
  try { return window.sessionStorage } catch { return null }
}

async function begin() {
  settleIntoTelegram()
  const store = storage()
  const data = launchData(location.hash, store)
  // The fragment is a credential for as long as it is good, so it leaves
  // the address bar — and the history entry — the moment it is read.
  if (location.hash) history.replaceState(null, '', location.pathname + location.search)
  if (!data) {
    say('Open Scout from Telegram', 'This page signs you in with the Scout bot. Open it from the Trips button in your chat with Scout.')
    return
  }
  if (justExchanged(store, Date.now())) {
    say('This browser will not keep you signed in here', 'Telegram Web in this browser blocks the cookie the trip page needs. Use the Telegram app, or open goodscout.fyi/chat in a tab of its own.')
    return
  }
  let res
  try {
    res = await fetch('/tg/session', {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        'x-scout-csrf': document.querySelector('meta[name="csrf"]').content,
      },
      body: JSON.stringify({ init_data: data }),
    })
  } catch {
    say('Could not reach Scout', 'Check your connection, then close this and open it again.')
    return
  }
  if (res.ok) {
    markExchanged(store, Date.now())
    location.replace(tripPageUrl(location.search))
    return
  }
  if (res.status === 403) {
    say('Scout is invite-only right now', 'Your trips open here once you have a seat. Send your invite code to the bot as /start your-code.')
    return
  }
  if (res.status === 401) {
    say('This link has expired', 'Close this and open Trips again from your chat with Scout.')
    return
  }
  say('Something went wrong on our side', 'Close this and open it again in a minute.')
}

begin()
