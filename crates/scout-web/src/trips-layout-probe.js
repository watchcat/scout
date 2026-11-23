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
// does and reports what showed (`taps`): as an iPhone taps and as an
// Android phone does, which differ (`tap`), and last as a mouse clicks.
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
const pause = (ms) => new Promise((done) => setTimeout(done, ms))
const until = async (ok) => {
  for (let i = 0; i < 400 && !ok(); i++) await pause(25)
  return ok()
}
const titled = (name) => () => document.querySelector('#trip-detail .trip-title')?.textContent === name
const listed = (name) => [...document.querySelectorAll('.trip-list button')]
  .find((one) => one.querySelector('.trip-list-name')?.textContent === name)

addEventListener('load', async () => {
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
  report.taps = {}
  for (const phone of ['iphone', 'android']) {
    listed(TAPPED)?.click()
    report.taps[phone] = (await until(titled(TAPPED)))
      ? await taps(phone)
      : { checked: 0, problems: [`${TAPPED} could not be shown to tap on`] }
  }
  report.taps.mouse = await mouseOnAnOption()
  document.documentElement.dataset.layout = JSON.stringify(report)
})

// A tap as a phone's browser sends one: the pointer's down and up from a
// finger, then the click. An iPhone's click says it came from a mouse
// (Safari 18.2 on: WebKit bug 282988, fixed and the fix reverted); an
// Android phone's says a finger. `phone` 'mouse' is a mouse's click.
// Returns whether the click was let through (`dispatchEvent`'s answer:
// false when the page prevented its default).
function tap(el, phone) {
  const rect = el.getBoundingClientRect()
  const at = (pointerType) => ({
    bubbles: true, cancelable: true, composed: true, isPrimary: true, pointerType,
    clientX: rect.left + rect.width / 2, clientY: rect.top + rect.height / 2,
  })
  const press = phone === 'mouse' ? 'mouse' : 'touch'
  el.dispatchEvent(new PointerEvent('pointerdown', at(press)))
  el.dispatchEvent(new PointerEvent('pointerup', at(press)))
  return el.dispatchEvent(new PointerEvent('click', at(phone === 'android' ? 'touch' : 'mouse')))
}

const tip = () => document.querySelector('.airport-tip:popover-open')
// Into view first, and past the scroll that moved it, which closes a name
// that is showing.
const ready = async (el) => {
  el.scrollIntoView({ block: 'center' })
  await pause(100)
}

// Taps, as `phone` taps, on the shown trip's codes: one in the segment's
// route, one inside a flight option not chosen, one in a day chip, one in
// another trip's row in the list; and a mouse's click on the route's.
// Each check that fails is a problem; `checked` counts the checks run, so
// a probe that ran none does not pass for one that found nothing wrong.
async function taps(phone) {
  const found = { checked: 0, problems: [] }
  const check = (ok, problem) => {
    found.checked++
    if (!ok) found.problems.push(problem)
  }

  // Looked up afresh each time: a redraw of the trip draws a new one, and
  // a tap on the old one, no longer on the page, reaches nothing.
  const routeCode = () => document.querySelector('#trip-detail .segment-route abbr.airport')
  const route = routeCode()
  if (!route) return { checked: 0, problems: ['no segment route has a named code'] }
  await ready(route)
  tap(route, phone)
  const shown = tip()
  check(shown, `a tap on ${route.textContent} in the route showed nothing`)
  if (shown) {
    found.shown = shown.textContent
    const box = shown.getBoundingClientRect()
    const code = route.getBoundingClientRect()
    const width = document.documentElement.clientWidth
    check(box.left >= 0 && box.right <= width && box.top >= 0 && box.bottom <= innerHeight,
      `the name spans ${Math.round(box.left)}–${Math.round(box.right)} × ${Math.round(box.top)}–${Math.round(box.bottom)} in a ${width}×${innerHeight} window`)
    check(box.top >= code.bottom - 0.5 || box.bottom <= code.top + 0.5, 'the name covers the code it names')
    check(box.left <= code.right && box.right >= code.left, 'the name is not beside the code it names')
  }
  tap(route, phone)
  check(!tip(), 'a second tap on the same code left its name showing')

  // Shown, then closed by each of the things that close it. Asked whether
  // it showed first, so a name that never opened does not pass for one
  // that closed.
  const closes = async (what, act) => {
    tap(routeCode(), phone)
    check(tip(), `a tap on ${route.textContent} did not show its name before ${what}`)
    await act()
    check(!tip(), `${what} left the name showing`)
  }
  await closes('a tap elsewhere', () => {
    document.body.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, pointerType: 'touch' }))
  })
  await closes('Escape', () => {
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
  })
  await closes('a scroll', () => {
    document.getElementById('trip-detail').dispatchEvent(new Event('scroll'))
  })
  await closes('a resize', () => {
    window.dispatchEvent(new Event('resize'))
  })
  // Clicked, not tapped, so nothing but the redraw and the tab can close it.
  await closes('a redraw of the trip', () => {
    listed(TAPPED).click()
  })
  await closes('a change of tab', async () => {
    document.getElementById('view-chat').click()
    document.getElementById('view-trips').click()
    await until(titled(TAPPED))
  })

  // In a flight option the code is named and the flight is not chosen.
  const option = document.querySelector('#trip-detail .flight-option:not(.selected) abbr.airport')
  check(option, 'no flight option that is not chosen has a named code')
  if (option) {
    await ready(option)
    const before = writes.length
    const through = tap(option, phone)
    check(tip()?.textContent.startsWith(option.textContent), `a tap on ${option.textContent} in a flight option did not name it`)
    check(!through && writes.length === before, `a tap on ${option.textContent} in a flight option chose the flight: ${writes.slice(before)}`)
    check(!option.closest('.flight-option').classList.contains('selected'), 'the flight option was marked chosen')
    tap(option, phone)
  }

  // A day chip's code is part of the chip: a tap does what a tap on the
  // chip does, moving to the chip's card, and names nothing.
  const chip = document.querySelector('#trip-detail a.day-chip abbr.airport')
  check(chip, 'no day chip has a named code')
  if (chip) {
    await ready(chip)
    tap(chip, phone)
    check(!tip(), `a tap on ${chip.textContent} in a day chip named it`)
    const card = chip.closest('a').hash.slice(1)
    check(document.activeElement?.id === card, `a tap on ${chip.textContent} in a day chip did not move to ${card}`)
  }

  // So is a trip's row in the list: a tap on its code opens that trip.
  const row = [...document.querySelectorAll('.trip-list button')]
    .find((one) => one.querySelector('.trip-list-name')?.textContent !== TAPPED && one.querySelector('abbr.airport'))
  check(row, 'no other trip in the list has a named code')
  if (row) {
    const name = row.querySelector('.trip-list-name').textContent
    tap(row.querySelector('abbr.airport'), phone)
    check(await until(titled(name)), `a tap on a code in ${name}'s row did not open it`)
    check(!tip(), `a tap on a code in ${name}'s row named it`)
    listed(TAPPED).click()
    await until(titled(TAPPED))
  }

  // A mouse has the hover title: its click shows nothing and goes on as
  // any click does.
  await ready(routeCode())
  const through = tap(routeCode(), 'mouse')
  check(!tip() && through, 'a mouse click on a code showed its name, or was stopped, as a tap is')
  return found
}

// A mouse's click on a code in a flight option still chooses the flight,
// as a click anywhere on the option does. Last, because it sends the
// choice; the app refuses it (`signed_in_site`), and nothing is measured
// after it.
async function mouseOnAnOption() {
  const found = { checked: 0, problems: [] }
  listed(TAPPED)?.click()
  await until(titled(TAPPED))
  const option = document.querySelector('#trip-detail .flight-option:not(.selected) abbr.airport')
  found.checked++
  if (!option) {
    found.problems.push('no flight option that is not chosen has a named code')
    return found
  }
  await ready(option)
  const before = writes.length
  tap(option, 'mouse')
  found.checked++
  if (!(await until(() => writes.slice(before).some((url) => url.endsWith('/chat/trips/choice'))))) {
    found.problems.push(`a mouse click on ${option.textContent} in a flight option did not choose it: ${writes.slice(before)}`)
  }
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
