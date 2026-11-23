// Test-only. `nothing_on_a_trip_is_wider_than_the_column_it_is_shown_in`
// (routes/trips.rs) serves this beside the real chat page and has Chrome
// dump the DOM once the budget it gives the page has run out. This opens
// Trips, opens each trip in the list in turn, measures what a reader would
// see cut, and leaves the findings on the root element as `data-layout`:
// for each trip, anything wider than the column or cut by the box that
// clips it, every outer shadow that reaches past that box, the title's
// width beside the column's, and what its free-day cards say.
//
// Served as a file of its own because the page's policy runs no inline
// script, and a plain one, so it is running before the page's module is.

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
  document.documentElement.dataset.layout = JSON.stringify(report)
})

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

  found.free = [...detail.querySelectorAll('.free-days-card')].map((card) => ({
    text: card.textContent,
    days: [...card.querySelectorAll('time')].map((time) => time.dateTime),
  }))
  const style = getComputedStyle(detail)
  found.column = detail.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight)
  found.title = detail.querySelector('.trip-head > :first-child').getBoundingClientRect().width
  return found
}
