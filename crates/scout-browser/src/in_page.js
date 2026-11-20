// What a session runs inside the page. Ported from scripts/shop-reach.mjs,
// the prototype the reading of 2026-11-18 was made with: outlineInPage is its
// function of that name, and the rest are its small helpers.
//
// It runs in a world of its own (Page.createIsolatedWorld), which shares the
// page's document and none of its script: the page cannot read or renumber
// what is kept here, and cannot hand this code a getAttribute of its making.

// The visible text in reading order, the pressable things numbered, whatever
// sits in front of the page first.
function outlineInPage(max, from) {
  const refs = (globalThis.__scoutRefs = {})
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
  const describe = (el) => {
    const tag = el.tagName
    if (tag === 'SELECT') {
      const options = [...el.options].slice(0, 15).map((o) => (o.selected ? '*' : '') + clean(o.textContent)).join(' | ')
      return `[${n}] list "${labelOf(el)}": ${options}`
    }
    if (tag === 'INPUT' || tag === 'TEXTAREA') {
      const type = (el.getAttribute('type') || 'text').toLowerCase()
      if (type === 'checkbox' || type === 'radio') return `[${n}] ${type}${el.checked ? ' (on)' : ''} "${labelOf(el)}"`
      if (type === 'submit' || type === 'button' || type === 'image' || type === 'reset') return `[${n}] button "${clean(el.value) || labelOf(el)}"`
      if (type === 'file') return `[${n}] file "${labelOf(el)}"`
      // What a password field holds is nobody's to read back.
      const value = type === 'password' ? '' : clean(el.value).slice(0, 40)
      return `[${n}] field "${labelOf(el)}"${value ? ` = "${value}"` : ''}`
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

// The outline, and the page's own markup for the one who decides whether it
// is a refusal: that is judged by the extractor, which reads markup.
function pageInPage(max, from) {
  const page = outlineInPage(max, from)
  page.html = document.documentElement ? document.documentElement.outerHTML : ''
  return page
}

// Where the document is, by the document.
function addressInPage() {
  return location.href
}

function labelOf(el) {
  const clean = (s) => (s || '').replace(/\s+/g, ' ').trim()
  const aria = el.getAttribute('aria-label')
  if (aria) return clean(aria)
  const named = namedBy(el)
  if (named) return named
  if (el.labels && el.labels.length) return clean(el.labels[0].innerText)
  return clean(el.getAttribute('placeholder') || el.getAttribute('title') || el.getAttribute('name') || '')
}

// What aria-labelledby points at, as text.
function namedBy(el) {
  const ids = (el.getAttribute('aria-labelledby') || '').split(/\s+/).filter(Boolean)
  const root = el.getRootNode()
  return ids.map((id) => (root.getElementById ? root.getElementById(id) : null)).filter(Boolean).map((e) => e.textContent).join(' ').replace(/\s+/g, ' ').trim()
}

function numbered(n) {
  const el = (globalThis.__scoutRefs || {})[n]
  return el && el.isConnected ? el : null
}

const NOT_NUMBERED = (n) => ({ error: `nothing is numbered ${n} on this page now; read it again` })

// What the thing numbered n is, and where a press on it would land.
//
// The prototype pressed the middle of the thing's box, and whatever lay over
// that point got the press. Here a point is only given if the thing itself,
// or its label, is what is there: the middle of the box first, then the
// middle of each line of a link that runs over several.
function targetInPage(n) {
  const el = numbered(n)
  if (!el) return NOT_NUMBERED(n)
  // A link or a form that wants a window of its own gets this one: a
  // session has one page, and the address it goes to is judged like any.
  if (el.tagName === 'A' && el.target && el.target !== '_self') el.target = '_self'
  if (el.form && el.form.target && el.form.target !== '_self') el.form.target = '_self'
  el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' })
  const within = (inner, outer) => {
    for (let e = inner; e; e = e.parentNode || e.host) if (e === outer) return true
    return false
  }
  const at = (x, y) => {
    let e = document.elementFromPoint(x, y)
    while (e && e.shadowRoot) {
      const inner = e.shadowRoot.elementFromPoint(x, y)
      if (!inner || inner === e) break
      e = inner
    }
    return e
  }
  const mine = (e) => {
    if (!e) return false
    if (within(e, el)) return true
    const label = e.closest ? e.closest('label') : null
    return !!label && label.control === el
  }
  const box = el.getBoundingClientRect()
  const rects = [box, ...[...el.getClientRects()].slice(0, 4)].filter((r) => r.width > 0 && r.height > 0)
  const points = rects.map((r) => ({ x: r.left + r.width / 2, y: r.top + r.height / 2 })).filter((p) => p.x >= 0 && p.y >= 0 && p.x < innerWidth && p.y < innerHeight)
  const point = points.find((p) => mine(at(p.x, p.y))) || null
  let covered = null
  if (!point && points.length) {
    const over = at(points[0].x, points[0].y)
    covered = over ? (over.innerText || over.getAttribute('aria-label') || over.tagName.toLowerCase()).replace(/\s+/g, ' ').trim().slice(0, 60) : ''
  }
  const type = (el.getAttribute('type') || '').toLowerCase()
  const label = el.labels && el.labels.length ? el.labels[0].innerText : ''
  return {
    tag: el.tagName,
    href: el.tagName === 'A' ? el.href : null,
    upload: el.tagName === 'INPUT' && type === 'file',
    sized: rects.length > 0,
    point,
    covered,
    field: {
      tag: el.tagName, type: el.getAttribute('type'), inputmode: el.getAttribute('inputmode'), autocomplete: el.getAttribute('autocomplete'),
      name: el.getAttribute('name'), id: el.id || null, placeholder: el.getAttribute('placeholder'), title: el.getAttribute('title'),
      label: [el.getAttribute('aria-label') || '', namedBy(el), label].join(' ').replace(/\s+/g, ' ').trim(),
    },
  }
}

// Puts the caret in the thing numbered n and says whether it is there: what
// is typed next goes to whatever has the focus, and a page can move it.
function focusInPage(n) {
  const el = numbered(n)
  if (!el) return false
  el.focus()
  if (el.select) el.select()
  let active = document.activeElement
  while (active && active.shadowRoot && active.shadowRoot.activeElement) active = active.shadowRoot.activeElement
  return active === el
}

// For a thing with no box to press: it is pressed as script presses it.
function pressInPage(n) {
  const el = numbered(n)
  if (el) el.click()
  return !!el
}

// Whether two wordings name the same option: a model writes a hyphen where
// the page has a dash, and drops a colon.
function sameOption(a, b) {
  const flat = (s) => String(s).toLowerCase().replace(/×/g, 'x').replace(/[^\p{L}\p{N}]/gu, '')
  return flat(a) !== '' && flat(a) === flat(b)
}

function chooseInPage(n, option) {
  const el = numbered(n)
  if (!el) return NOT_NUMBERED(n)
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
