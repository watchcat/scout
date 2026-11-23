// Test-only. `nothing_on_a_trip_is_wider_than_the_column_it_is_shown_in`
// (routes/trips.rs) serves this beside the real chat page and has Chrome
// dump the DOM once the budget it gives the page has run out. This opens
// Trips, opens each trip in the list in turn, measures what a reader would
// see cut, and leaves the findings on the root element as `data-layout`:
// for each trip, anything wider than the column or cut by the box that
// clips it, every outer shadow that reaches past that box, the title's
// width beside the column's, what its free-day cards say, and the names
// its airport codes show.
//
// Then it taps airport codes on the trip named `TAPPED` the way a phone
// does and reports what showed (`taps`).
//
// Served as a file of its own because the page's policy runs no inline
// script, and a plain one, so it is running before the page's module is:
// early enough to see every request the page sends that is not a read.

const TAPPED = 'Amsterdam to Yerevan'
const writes = []
const pageFetch = window.fetch
window.fetch = (input, init) => {
  if ((init?.method ?? 'GET').toUpperCase() !== 'GET') writes.push(String(input))
  return pageFetch(input, init)
}

addEventListener('load', async () => {
  const pause = (ms) => new Promise((done) => setTimeout(done, ms))
  const until = async (ok) => {
    for (let i = 0; i < 400 && !ok(); i++) await pause(25)
    return ok()
  }
  const report = { width: innerWidth, trips: [] }
  document.getElementById('view-trips').click()
  if (!(await until(() => document.querySelector('#trip-detail .trip-head')))) {
    report.error = 'no trip was shown'
  }
  // The airport names arrive on a request of their own, once: waited for
  // here, so a trip that draws no code is not waited on for one.
  if (!(await until(() => document.querySelector('abbr.airport')))) {
    report.airportsNeverNamed = true
  }
  for (const button of document.querySelectorAll('.trip-list button')) {
    const name = button.querySelector('.trip-list-name')?.textContent
    button.click()
    const shown = () => document.querySelector('#trip-detail .trip-title')?.textContent === name
    if (!(await until(shown))) {
      report.trips.push({ name, problems: [`${name} was never shown`] })
      continue
    }
    await pause(50)
    report.trips.push(measure(name))
  }
  const button = [...document.querySelectorAll('.trip-list button')]
    .find((one) => one.querySelector('.trip-list-name')?.textContent === TAPPED)
  button?.click()
  if (button && (await until(() => document.querySelector('#trip-detail .trip-title')?.textContent === TAPPED))) {
    report.taps = await taps(pause)
  } else {
    report.taps = { checked: 0, problems: [`${TAPPED} could not be shown to tap on`] }
  }
  document.documentElement.dataset.layout = JSON.stringify(report)
})

// A tap as a phone's browser sends one: the pointer's down and up, then
// the click, all from a finger. Returns whether the click was let through
// (`dispatchEvent`'s answer: false when the page prevented its default).
function tap(el, pointerType = 'touch') {
  const rect = el.getBoundingClientRect()
  const at = {
    bubbles: true, cancelable: true, composed: true, isPrimary: true, pointerType,
    clientX: rect.left + rect.width / 2, clientY: rect.top + rect.height / 2,
  }
  el.dispatchEvent(new PointerEvent('pointerdown', at))
  el.dispatchEvent(new PointerEvent('pointerup', at))
  return el.dispatchEvent(new PointerEvent('click', at))
}

// Taps on the shown trip's codes: one in the segment's route, one inside a
// flight option not chosen, one in a day chip, one with a mouse. Each
// check that fails is a problem; `checked` counts the checks run, so a
// probe that ran none does not pass for one that found nothing wrong.
async function taps(pause) {
  const found = { checked: 0, problems: [] }
  const check = (ok, problem) => {
    found.checked++
    if (!ok) found.problems.push(problem)
  }
  const tip = () => document.querySelector('.airport-tip:popover-open')
  // Into view first, and past the scroll that moved it, which closes a
  // name that is showing.
  const ready = async (el) => {
    el.scrollIntoView({ block: 'center' })
    await pause(100)
  }

  const route = document.querySelector('#trip-detail .segment-route abbr.airport')
  if (!route) return { checked: 0, problems: ['no segment route has a named code'] }
  await ready(route)
  tap(route)
  const shown = tip()
  check(shown, `a tap on ${route.textContent} in the route showed nothing`)
  if (shown) {
    found.shown = shown.textContent
    check(shown.getAttribute('role') === 'tooltip', `the name has role ${shown.getAttribute('role')}`)
    const box = shown.getBoundingClientRect()
    const code = route.getBoundingClientRect()
    const width = document.documentElement.clientWidth
    check(box.left >= 0 && box.right <= width && box.top >= 0 && box.bottom <= innerHeight,
      `the name spans ${Math.round(box.left)}–${Math.round(box.right)} × ${Math.round(box.top)}–${Math.round(box.bottom)} in a ${width}×${innerHeight} window`)
    check(box.top >= code.bottom - 0.5 || box.bottom <= code.top + 0.5, 'the name covers the code it names')
    check(box.left <= code.right && box.right >= code.left, 'the name is not beside the code it names')
  }
  tap(route)
  check(!tip(), 'a second tap on the same code left its name showing')

  tap(route)
  document.body.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, pointerType: 'touch' }))
  check(!tip(), 'a tap elsewhere left the name showing')
  tap(route)
  document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
  check(!tip(), 'Escape left the name showing')
  tap(route)
  document.getElementById('trip-detail').dispatchEvent(new Event('scroll'))
  check(!tip(), 'a scroll left the name showing')

  // In a flight option the code is named and the flight is not chosen.
  const option = document.querySelector('#trip-detail .flight-option:not(.selected) abbr.airport')
  check(option, 'no flight option that is not chosen has a named code')
  if (option) {
    await ready(option)
    const before = writes.length
    const through = tap(option)
    check(tip()?.textContent.startsWith(option.textContent), `a tap on ${option.textContent} in a flight option did not name it`)
    check(!through && writes.length === before, `a tap on ${option.textContent} in a flight option chose the flight: ${writes.slice(before)}`)
    check(!option.closest('.flight-option').classList.contains('selected'), 'the flight option was marked chosen')
    tap(option)
  }

  // A day chip's code is part of the chip: a tap does what a tap on the
  // chip does, and names nothing.
  const chip = document.querySelector('#trip-detail a.day-chip abbr.airport')
  check(chip, 'no day chip has a named code')
  if (chip) {
    await ready(chip)
    tap(chip)
    check(!tip(), `a tap on ${chip.textContent} in a day chip named it`)
  }

  // A mouse has the hover title, and its click is left alone.
  await ready(route)
  tap(route, 'mouse')
  check(!tip(), 'a mouse click on a code showed its name as a tap does')
  return found
}

// The nearest box above `el` that clips it sideways: one that scrolls,
// or hides, on the x axis. A box with `overflow-y` set clips x as well.
function clipper(el) {
  for (let up = el.parentElement; up; up = up.parentElement) {
    if (getComputedStyle(up).overflowX !== 'visible') return up
  }
  return document.documentElement
}

// What of `el` is drawn: its padding box, in the viewport's terms. A
// child's shadow and border are cut at this edge.
function drawn(el) {
  if (el === document.documentElement) return { left: 0, right: document.documentElement.clientWidth }
  const rect = el.getBoundingClientRect()
  const left = rect.left + parseFloat(getComputedStyle(el).borderLeftWidth)
  return { left, right: left + el.clientWidth }
}

// `box-shadow` as computed: "rgba(0, 0, 0, 0.16) 0px 10px 32px 0px, …".
function shadows(value) {
  if (!value || value === 'none') return []
  return value.split(/,(?![^(]*\))/).map((one) => {
    const lengths = (one.replace(/rgba?\([^)]*\)/g, '').match(/-?[\d.]+px/g) || []).map(parseFloat)
    const [x = 0, y = 0, blur = 0, spread = 0] = lengths
    return { inset: /\binset\b/.test(one), x, y, blur, spread }
  })
}

function measure(name) {
  const detail = document.getElementById('trip-detail')
  const found = { name, problems: [] }
  const px = (n) => Math.round(n)

  for (const el of [detail, document.getElementById('trips-view'), document.documentElement]) {
    const over = el.scrollWidth - el.clientWidth
    if (over > 0) found.problems.push(`#${el.id || el.tagName.toLowerCase()} is ${over}px wider inside than it shows`)
  }

  // Everything drawn, against the box that cuts it: the column itself, or
  // a card that hides what passes its edge. A strip made to scroll
  // sideways (the overview's points) is the one box whose content may run
  // past it, since a reader can scroll to the rest.
  const columns = new Set([detail, document.getElementById('trips-view'), document.documentElement])
  for (const el of detail.querySelectorAll('*')) {
    const rect = el.getBoundingClientRect()
    if (!rect.width || !rect.height) continue
    const cut = clipper(el)
    const scrolls = ['auto', 'scroll'].includes(getComputedStyle(cut).overflowX)
    if (scrolls && !columns.has(cut)) continue
    const edge = drawn(cut)
    if (rect.left < edge.left - 0.5 || rect.right > edge.right + 0.5) {
      const classes = el.getAttribute('class')
      const what = `<${el.tagName.toLowerCase()}${classes ? ` class="${classes}"` : ''}>`
      found.problems.push(`${what} "${el.textContent.trim().slice(0, 40)}" spans ${px(rect.left)}–${px(rect.right)}, and is cut at ${px(edge.left)}–${px(edge.right)}`)
    }
  }

  // A blurred shadow reaches its blur radius past the shadow's edge, which
  // its spread moves out and its offset moves sideways.
  for (const el of detail.querySelectorAll('*')) {
    const rect = el.getBoundingClientRect()
    if (!rect.width) continue
    for (const shadow of shadows(getComputedStyle(el).boxShadow)) {
      if (shadow.inset) continue
      const left = rect.left - (shadow.blur + shadow.spread - shadow.x)
      const right = rect.right + (shadow.blur + shadow.spread + shadow.x)
      const edge = drawn(clipper(el))
      if (left < edge.left - 0.5 || right > edge.right + 0.5) {
        found.problems.push(`.${el.getAttribute('class')}'s shadow reaches ${px(left)}–${px(right)}, and is cut at ${px(edge.left)}–${px(edge.right)}`)
      }
    }
  }

  // Each code the trip draws, with the name it shows on hover, and any
  // element drawn from codes that still shows one bare.
  found.airports = Object.fromEntries([...document.querySelectorAll('#trip-detail abbr.airport, .trip-list [aria-current="true"] abbr.airport')]
    .map((abbr) => [abbr.textContent, abbr.title]))
  found.bare = []
  for (const el of document.querySelectorAll('#trip-detail [data-codes], .trip-list [aria-current="true"] [data-codes]')) {
    const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT)
    while (walker.nextNode()) {
      const text = walker.currentNode
      if (!text.parentElement.closest('abbr') && /\b[A-Z]{3}\b/.test(text.data)) found.bare.push(`${el.getAttribute('class') ?? el.tagName}: ${text.data}`)
    }
  }
  found.free = [...detail.querySelectorAll('.free-days-card')].map((card) => ({
    text: card.textContent,
    days: [...card.querySelectorAll('time')].map((time) => time.dateTime),
  }))
  const style = getComputedStyle(detail)
  found.column = detail.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight)
  found.title = detail.querySelector('.trip-head > :first-child').getBoundingClientRect().width
  return found
}
