// What a session runs inside the page. Ported from scripts/shop-reach.mjs,
// the prototype the reading of 2026-11-18 was made with: outlineInPage is its
// function of that name, and the rest are its small helpers.
//
// It runs in a world of its own (Page.createIsolatedWorld), which shares the
// page's document and none of its script: the page cannot read or renumber
// what is kept here, and cannot hand this code a getAttribute of its making.

// Everything of the page's own writing that is handed on is cut to a length:
// the name of a link or of a field is a few words, whatever the page wrote.
// The first three are the prototype's; the rest are chosen, not measured.
const LINK = 120
const VALUE = 40
const OPTIONS = 15
const LABEL = 80
const OPTION = 60
const COVERED = 60
const SAID_OF_A_FIELD = 200

// The first n of s, never ending on half a letter. A letter outside the
// first 65,536 is two units of a string here, and a cut between them leaves
// a half that is no text at all: the answer it was in could not be read.
function cut(s, n) {
  if (s.length <= n) return s
  const last = s.charCodeAt(n - 1)
  return s.slice(0, last >= 0xd800 && last <= 0xdbff ? n - 1 : n)
}

// s with each half letter of the page's own making turned into the mark for
// a letter that could not be read.
function sound(s) {
  return typeof s.toWellFormed === 'function' ? s.toWellFormed() : s.replace(/[\ud800-\udbff](?![\udc00-\udfff])|(?<![\ud800-\udbff])[\udc00-\udfff]/g, '�')
}

const clean = (s) => (s || '').replace(/\s+/g, ' ').trim()

// How an element is shown, or null where a person would not see it: taken
// out of the page, made transparent, put where no scrolling brings it, or
// cut down to a dot, which is how words are kept for a screen reader alone.
// `tiny` says its own text has no size.
//
// Not everything a page can hide is told this way. Text in the colour of
// its ground, or with something laid over it, is read like any other.
function lookOf(el) {
  const style = getComputedStyle(el)
  if (style.display === 'none' || style.visibility === 'hidden' || style.visibility === 'collapse') return null
  if (parseFloat(style.opacity) === 0) return null
  const look = { tiny: parseFloat(style.fontSize) === 0 }
  if (style.display === 'contents') return look
  if (el.getClientRects().length === 0) return null
  const box = el.getBoundingClientRect()
  if (box.right + scrollX <= 0 || box.bottom + scrollY <= 0) return null
  if ((box.width <= 1 || box.height <= 1) && (style.overflowX !== 'visible' || style.overflowY !== 'visible')) return null
  return look
}

// The boxes of an element that a person could put a pointer on.
function boxesOf(el) {
  if (!lookOf(el)) return []
  // Its own look says nothing of what it stands in: one made transparent
  // by something around it is not seen either.
  if (typeof el.checkVisibility === 'function' && !el.checkVisibility({ opacityProperty: true, visibilityProperty: true, checkOpacity: true, checkVisibilityCSS: true })) return []
  return [el.getBoundingClientRect(), ...[...el.getClientRects()].slice(0, 4)].filter((r) => r.width > 1 && r.height > 1)
}

// Where a thing can be pressed: on itself, or, for a tick box or a choice
// that a page draws by its label and hides, on that label.
function pressBoxesOf(el) {
  const own = boxesOf(el)
  if (own.length || !el.labels) return own
  return [...el.labels].flatMap(boxesOf)
}

// What a thing that can be pressed is: its kind and its words, as the
// outline writes it after the number. Null where it says nothing at all.
function saidOf(el) {
  const tag = el.tagName
  if (tag === 'SELECT') {
    const options = [...el.options].slice(0, OPTIONS).map((o) => (o.selected ? '*' : '') + cut(clean(o.textContent), OPTION)).join(' | ')
    return `list "${labelOf(el)}": ${options}`
  }
  if (tag === 'INPUT' || tag === 'TEXTAREA') {
    const type = (el.getAttribute('type') || 'text').toLowerCase()
    if (type === 'checkbox' || type === 'radio') return `${type}${el.checked ? ' (on)' : ''} "${labelOf(el)}"`
    if (type === 'submit' || type === 'button' || type === 'image' || type === 'reset') return `button "${cut(clean(el.value), LABEL) || labelOf(el)}"`
    if (type === 'file') return `file "${labelOf(el)}"`
    // What a password field holds is nobody's to read back.
    const value = type === 'password' ? '' : cut(clean(el.value), VALUE)
    return `field "${labelOf(el)}"${value ? ` = "${value}"` : ''}`
  }
  const img = el.querySelector('img[alt]')
  const text = cut(clean(el.innerText), LINK) || cut(clean(el.getAttribute('aria-label') || el.getAttribute('title') || (img ? img.alt : '')), LINK)
  if (!text) return null
  const on = ['aria-pressed', 'aria-selected', 'aria-checked'].some((a) => el.getAttribute(a) === 'true') ? ' (on)' : ''
  const off = el.disabled || el.getAttribute('aria-disabled') === 'true' ? ' (off)' : ''
  return `${tag === 'A' ? 'link' : 'button'}${on}${off} "${text}"`
}

// The page's text in reading order, less what it hides (see lookOf), with
// the things that can be pressed numbered, and whatever sits in front of
// the page first. All of it, up to caps.whole: it is handed out in parts
// by the one who asked, so that reading on never numbers anything again.
function outlineInPage(caps) {
  const refs = (globalThis.__scoutRefs = {})
  let n = 0
  const parts = []
  const walked = new WeakSet()
  const SKIP = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'IFRAME', 'FRAME', 'OBJECT', 'EMBED', 'CANVAS', 'VIDEO', 'AUDIO', 'HEAD'])
  const BLOCK = new Set(['DIV', 'P', 'LI', 'UL', 'OL', 'SECTION', 'ARTICLE', 'MAIN', 'HEADER', 'FOOTER', 'NAV', 'ASIDE', 'H1', 'H2', 'H3', 'H4', 'H5', 'H6', 'TR', 'TABLE', 'FORM', 'FIELDSET', 'DL', 'DT', 'DD', 'BR', 'HR', 'DIALOG', 'DETAILS', 'LABEL'])
  const PRESS = 'a[href],button,input:not([type=hidden]),select,textarea,summary,[role=button],[role=link],[role=radio],[role=option],[role=tab],[role=checkbox],[role=menuitem],[role=switch],[role=combobox]'
  const walk = (node) => {
    if (node.nodeType !== 1 || walked.has(node)) return
    walked.add(node)
    if (SKIP.has(node.tagName) || node.namespaceURI !== 'http://www.w3.org/1999/xhtml') return
    if (node.getAttribute('aria-hidden') === 'true') return
    if (node.matches(PRESS)) {
      // A thing nobody can see is given no number. Nor is one that only
      // wraps a frame: what is in a frame is not read, and a press on the
      // wrapping would press something in it that nobody was shown.
      if (node.querySelector('iframe,frame,object,embed') || pressBoxesOf(node).length === 0) return
      const said = saidOf(node)
      if (!said) return
      n += 1
      refs[n] = node
      parts.push(` [${n}] ${said} `)
      return
    }
    const look = lookOf(node)
    if (!look) return
    for (const child of node.shadowRoot ? node.shadowRoot.childNodes : node.childNodes) {
      // As written, spaces and all: "259" and ",-" in two spans are one amount.
      if (child.nodeType === 3) { if (!look.tiny) parts.push(child.nodeValue) } else walk(child)
    }
    if (BLOCK.has(node.tagName)) parts.push('\n')
  }
  const section = (title, roots) => {
    const before = parts.length
    for (const root of roots) walk(root)
    if (parts.slice(before).join('').trim()) parts.splice(before, 0, `\n# ${title}\n`)
  }
  const front = [...document.querySelectorAll('dialog[open],[role=dialog],[role=alertdialog],[aria-modal=true],[id*=cookie i],[class*=cookie i],[id*=consent i],[class*=consent i],[id*=onetrust i]')].filter((e) => lookOf(e))
  section('In front of the page', front)
  section('The page', [...document.querySelectorAll('main,[role=main]')])
  section('Around it', document.body ? [document.body] : [])
  const text = sound(parts.join('').replace(/[^\S\n]+/g, ' ').replace(/ ?\n ?/g, '\n').replace(/\n{2,}/g, '\n').trim())
  return { title: cut(sound(document.title), caps.title), url: cut(location.href, caps.address), at: location.href, text: cut(text, caps.whole), cut: text.length > caps.whole }
}

// The outline, and the page's own markup for the one who decides whether it
// is a refusal: that is judged by the extractor, which reads markup.
function pageInPage(caps) {
  const page = outlineInPage(caps)
  page.html = document.documentElement ? cut(sound(document.documentElement.outerHTML), caps.markup) : ''
  return page
}

// Where the document is, by the document.
function addressInPage() {
  return location.href
}

function labelOf(el) {
  const aria = el.getAttribute('aria-label')
  if (aria) return cut(clean(aria), LABEL)
  const named = namedBy(el)
  if (named) return cut(named, LABEL)
  if (el.labels && el.labels.length) return cut(clean(el.labels[0].innerText), LABEL)
  return cut(clean(el.getAttribute('placeholder') || el.getAttribute('title') || el.getAttribute('name') || ''), LABEL)
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

// What is at a point of the page, through whatever keeps its own inside.
function at(x, y) {
  let e = document.elementFromPoint(x, y)
  while (e && e.shadowRoot) {
    const inner = e.shadowRoot.elementFromPoint(x, y)
    if (!inner || inner === e) break
    e = inner
  }
  return e
}

// Whether a press that lands on `hit` is a press on `el`: on the thing
// itself, on something in it, or on its label. Never on a frame, even one
// inside it: a press there goes into the frame.
function isPressOn(el, hit) {
  if (!hit || ['IFRAME', 'FRAME', 'OBJECT', 'EMBED'].includes(hit.tagName)) return false
  for (let e = hit; e; e = e.parentNode || e.host) if (e === el) return true
  const label = hit.closest ? hit.closest('label') : null
  return !!label && label.control === el
}

const wordsOf = (e) => cut(clean(e.innerText || (e.getAttribute && e.getAttribute('aria-label')) || (e.tagName || 'the page').toLowerCase()), COVERED)

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
  const points = pressBoxesOf(el).map((r) => ({ x: r.left + r.width / 2, y: r.top + r.height / 2 })).filter((p) => p.x >= 0 && p.y >= 0 && p.x < innerWidth && p.y < innerHeight)
  if (points.length === 0) return { error: `number ${n} is not to be seen on the page now; read it again` }
  const point = points.find((p) => isPressOn(el, at(p.x, p.y))) || null
  const over = point ? null : at(points[0].x, points[0].y)
  return {
    href: el.tagName === 'A' ? el.href : null,
    upload: el.tagName === 'INPUT' && (el.getAttribute('type') || '').toLowerCase() === 'file',
    point,
    covered: point ? null : over ? wordsOf(over) : '',
    said: saidOf(el) || '',
    field: fieldOf(el),
  }
}

// Whether a press at a point would still land on the thing numbered n, now
// that the pointer is there: its arriving is something a page hears, and a
// page can answer it by putting something else under it.
function hitInPage(n, x, y) {
  const el = numbered(n)
  if (!el) return { on: false, over: '' }
  const hit = at(x, y)
  return { on: isPressOn(el, hit), over: hit ? wordsOf(hit) : '' }
}

// Listens for where the next press lands, since between the pointer going
// down and coming up nothing can be asked of the page.
function armInPage(n) {
  const el = numbered(n)
  if (!el) return false
  const took = { down: null, click: null }
  const who = (event) => {
    const hit = event.composedPath ? event.composedPath()[0] : event.target
    return isPressOn(el, hit) ? 'it' : wordsOf(hit) || 'something else'
  }
  // The first of each that the browser itself made is the press. What a
  // page's own script presses in answer (a button that opens a file field,
  // say) is the page's doing and is judged where it leads.
  const down = (event) => { if (event.isTrusted && took.down === null) took.down = who(event) }
  const click = (event) => { if (event.isTrusted && took.click === null) took.click = who(event) }
  addEventListener('mousedown', down, true)
  addEventListener('click', click, true)
  took.off = () => { removeEventListener('mousedown', down, true); removeEventListener('click', click, true) }
  globalThis.__scoutPress = took
  return true
}

// Where the press landed: 'it' for the thing it was meant for, the words
// of whatever else took it, or null for what was not heard at all.
function pressedInPage() {
  const took = globalThis.__scoutPress
  if (!took) return null
  took.off()
  globalThis.__scoutPress = null
  return { down: took.down, click: took.click }
}

// What the page says of a thing as a field: what the rule on typing reads.
// All of each, up to a length no honest name of a field comes near; a word
// that makes a field personal is looked for anywhere in it.
function fieldOf(el) {
  const said = (v) => (v == null ? null : cut(sound(String(v)), SAID_OF_A_FIELD))
  const label = el.labels && el.labels.length ? el.labels[0].innerText : ''
  return {
    tag: el.tagName, type: said(el.getAttribute('type')), inputmode: said(el.getAttribute('inputmode')), autocomplete: said(el.getAttribute('autocomplete')),
    name: said(el.getAttribute('name')), id: said(el.id || null), placeholder: said(el.getAttribute('placeholder')), title: said(el.getAttribute('title')),
    label: said(clean([el.getAttribute('aria-label') || '', namedBy(el), label].join(' '))),
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
  if (!found) return { error: `that list has no such option; it has: ${[...el.options].slice(0, OPTIONS).map((o) => cut(flat(o.textContent), OPTION)).join(' | ')}` }
  el.value = found.value
  el.dispatchEvent(new Event('input', { bubbles: true }))
  el.dispatchEvent(new Event('change', { bubbles: true }))
  return { chosen: cut(flat(found.textContent), OPTION) }
}
