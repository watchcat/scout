// The chat page's client. Pure helpers first (exported for
// `chat.test.mjs`), then the DOM wiring that uses them.

/// Moves accumulated text forward by one `TextUpdate`. The counterpart of
/// `TextUpdate::apply` in scout-api, and the reason it exists at all: a
/// `</think>` with no opener retracts everything already sent by replacing
/// it with `""`, and a client that only ever appended would leave that
/// stripped reasoning on screen forever. `Replace` must clear, not extend.
export function applyUpdate(answer, update) {
  if ('Append' in update) return answer + update.Append
  if ('Replace' in update) return update.Replace
  return answer
}

// What the bubble should show once the stream's `end` frame arrives.
//
// The bubble up to this point was built from token deltas, and those are
// every turn of a multi-turn run concatenated — including the "let me check
// the next shop" narration the model writes between tool calls, and any
// link the run's dead-link repair removed afterwards. The `end` frame
// carries what the run actually answered, so an `ok` end replaces the
// bubble outright. Replacing with an empty string is deliberate: it means
// the run produced nothing but reasoning, and the bubble has to clear.
// A `busy` or `error` end clears it for the same reason — neither produced
// an answer, and the notice beside it is the whole message.
//
// A missing `answer` is a different instruction from an empty one, and is
// not a retraction. A server part-way through a rollout still sends
// `{"status":"ok"}` on its own, and blanking a good answer because of a
// deploy would be worse than showing what streamed.
export function finalAnswer(end, streamed) {
  if (!end) return streamed
  if (end.status === 'ok') {
    return typeof end.answer === 'string' ? end.answer : streamed
  }
  // A run that failed, or never started, produced no answer. What streamed
  // is the model's working — the narration it writes between tool calls —
  // and leaving that above an apology reads as an answer cut off
  // mid-sentence rather than as nothing.
  if (end.status === 'error' || end.status === 'busy') return ''
  // An end this client does not recognise is not a reason to throw away
  // what the reader can already see.
  return streamed
}

// `&` first, or escaping `<`/`>` into `&lt;`/`&gt;` would itself get its
// `&` escaped a second time.
export function escapeHtml(text) {
  return text.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;')
}

// Trailing punctuation is trimmed off the link so a URL at the end of a
// sentence ("see https://example.com.") doesn't swallow the period into
// the href.
const URL_RE = /https?:\/\/[^\s<>"]+/g
const TRAILING_PUNCTUATION_RE = /[.,;:!?)\]}'"]+$/

// Must run after `escapeHtml`, never before: escaping markup that linkify
// had already produced would mangle the anchor tags it just wrote.
export function linkify(html) {
  return html.replace(URL_RE, (url) => {
    const trail = url.match(TRAILING_PUNCTUATION_RE)?.[0] ?? ''
    const href = trail ? url.slice(0, -trail.length) : url
    if (!href) return url
    return `<a href="${href}" rel="noopener noreferrer" target="_blank">${href}</a>${trail}`
  })
}

function render(text) {
  return linkify(escapeHtml(text))
}

// How close to the bottom still counts as "following along". 32px rather
// than 0 because a reader who nudged the wheel one line has not stopped
// following, and a strict comparison would strand them a pixel short and
// never scroll again.
const FOLLOW_SLACK = 32

// Whether new content should be scrolled into view. Pure, so the rule that
// makes a streaming answer bearable can be tested without a browser.
// Splits one SSE block ("event: ...\ndata: ...") into its two fields, or
// null when the block carries no event. Multiple `data:` lines are legal
// SSE and get joined with `\n`, though this protocol only ever sends one.
//
// Null is the common case as well as the error case: the server keeps the
// stream alive through a silent run by sending comment blocks, and a
// comment has no `event:` line. Lifted out of `start` so it can be tested —
// it touches no DOM.
export function parseFrame(block) {
  let event = null
  const dataLines = []
  for (const line of block.split('\n')) {
    if (line.startsWith('event:')) event = line.slice('event:'.length).trim()
    else if (line.startsWith('data:')) dataLines.push(line.slice('data:'.length).trim())
  }
  if (!event) return null
  return { event, data: dataLines.join('\n') }
}

export function shouldFollow(scrollTop, clientHeight, scrollHeight, slack = FOLLOW_SLACK) {
  return scrollHeight - scrollTop - clientHeight <= slack
}

// Tallest the composer may grow, in px — about five lines at this font.
// Matches the `max-height` in chat.html; if one changes the other must.
const COMPOSER_CAP = 200

export function composerHeight(scrollHeight, cap = COMPOSER_CAP) {
  return Math.min(scrollHeight, cap)
}

// The idle window after which an unpinned thread is deleted, and the point
// at which the sidebar starts saying so. Both mirror core: 48h expiry in
// `Core::THREAD_IDLE_SECS`, and "worth warning" at 36h.
const EXPIRES_AFTER_MS = 48 * 3600 * 1000
const WARN_AFTER_MS = 36 * 3600 * 1000

export function threadLabel(thread) {
  return thread.title ? { text: thread.title, unnamed: false } : { text: 'New thread', unnamed: true }
}

// "2h", "4d", or "expires in 12h" once an unpinned thread is close to
// going — so nobody learns about expiry by losing something.
export function whenLabel(thread, now = Date.now()) {
  // NaN propagates silently through every comparison below, so an
  // unparseable timestamp would reach the row as "NaNd". Saying nothing is
  // the honest answer: the row still has its title and its controls.
  const then = Date.parse(thread.updated_at)
  if (Number.isNaN(then)) return { text: '', expiring: false }
  const age = Math.max(0, now - then)
  if (!thread.pinned && age >= WARN_AFTER_MS) {
    const left = Math.max(1, Math.ceil((EXPIRES_AFTER_MS - age) / 3600000))
    return { text: `expires in ${left}h`, expiring: true }
  }
  const minutes = Math.floor(age / 60000)
  if (minutes < 5) return { text: 'now', expiring: false }
  if (minutes < 60) return { text: `${minutes}m`, expiring: false }
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return { text: `${hours}h`, expiring: false }
  return { text: `${Math.floor(hours / 24)}d`, expiring: false }
}

// The composer's request body. Named rather than inlined so the one place
// the thread id crosses the wire is the one place a test can hold.
export function sendBody(text, thread) {
  return JSON.stringify({ text, thread })
}

function start() {
  const csrfToken = document.querySelector('meta[name="csrf"]').content
  const turnsEl = document.getElementById('turns')
  const statusEl = document.getElementById('status')
  const noticeEl = document.getElementById('notice')
  const askForm = document.getElementById('ask')
  const textEl = document.getElementById('text')
  const sendButton = document.getElementById('send')
  const resetForm = document.getElementById('reset')
  const mirrorButton = document.getElementById('mirror')
  const sideEl = document.getElementById('side')
  const threadsEl = document.getElementById('threads')
  const menuButton = document.getElementById('menu')
  // The thread the page is showing. Every message names it, so a thread
  // the phone started meanwhile cannot swallow a message meant for this one.
  let currentThread = null

  // Enter sends, Shift+Enter is a newline. `requestSubmit` rather than
  // `submit` because it runs the form's own validation — so Enter on an
  // empty box does nothing, which is the guard the button already relied
  // on via `required`.
  textEl.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      askForm.requestSubmit()
    }
  })

  // Grow to fit, then stop. Reset to `auto` first or `scrollHeight` keeps
  // reporting the height already set and the box only ever grows.
  function fitComposer() {
    textEl.style.height = 'auto'
    textEl.style.height = `${composerHeight(textEl.scrollHeight)}px`
  }
  textEl.addEventListener('input', fitComposer)

  // Decide *before* appending, act after: once the new content is in the
  // DOM the reader's position looks different and the question cannot be
  // asked honestly any more.
  function following() {
    return shouldFollow(turnsEl.scrollTop, turnsEl.clientHeight, turnsEl.scrollHeight)
  }
  function follow(wasFollowing) {
    if (wasFollowing) turnsEl.scrollTop = turnsEl.scrollHeight
  }

  function turnElement(role, text) {
    const li = document.createElement('li')
    li.className = role === 'You' ? 'you' : 'scout'
    li.innerHTML = render(text)
    return li
  }

  // The status box is capped at a quarter of the viewport, so on a long
  // run the newest reasoning is the part below the fold. Tail it — but ask
  // the same question `.turns` asks, and for the same reason: a reader who
  // scrolled up to read something should not be dragged back down by the
  // next token. While hidden every measurement is 0, which reads as "at the
  // bottom", so the first status of a run always follows.
  function showStatus(text) {
    const wasFollowing = shouldFollow(
      statusEl.scrollTop, statusEl.clientHeight, statusEl.scrollHeight)
    statusEl.textContent = text
    statusEl.hidden = false
    if (wasFollowing) statusEl.scrollTop = statusEl.scrollHeight
    statusEl.classList.toggle('long', statusEl.scrollHeight > statusEl.clientHeight)
  }

  function hideStatus() {
    statusEl.hidden = true
    statusEl.textContent = ''
    statusEl.classList.remove('long')
  }

  function showNotice(text) {
    noticeEl.textContent = text
    noticeEl.hidden = false
  }

  function hideNotice() {
    noticeEl.hidden = true
    noticeEl.textContent = ''
  }

  async function loadHistory() {
    const res = await fetch('/chat/history')
    if (!res.ok) {
      showNotice('Could not load the conversation so far. Reload to try again.')
      return
    }
    showTurns(await res.json())
    await refreshThreads()
  }

  // Replaces the transcript wholesale rather than appending: every caller
  // is switching what the page is showing, not adding to it.
  function showTurns(turns) {
    turnsEl.replaceChildren()
    for (const turn of turns) turnsEl.append(turnElement(turn.role, turn.text))
    turnsEl.scrollTop = turnsEl.scrollHeight
  }

  // Every thread route is a POST behind the CSRF header, and most carry no
  // body at all — so the header set lives here once rather than at each of
  // the eight call sites.
  async function post(path, body) {
    return fetch(path, {
      method: 'POST',
      headers: { 'content-type': 'application/json', 'x-scout-csrf': csrfToken },
      body: body === undefined ? undefined : JSON.stringify(body),
    })
  }

  // The server is the authority on which thread is current, so the list is
  // where `currentThread` comes from — never from what this page last did.
  async function refreshThreads() {
    const res = await fetch('/chat/threads')
    if (!res.ok) return
    const list = await res.json()
    const current = list.find((t) => t.current)
    currentThread = current ? current.id : null
    renderThreads(list)
  }

  function renderThreads(list) {
    threadsEl.replaceChildren()
    for (const thread of list) threadsEl.append(threadRow(thread))
  }

  // Inline SVG rather than an emoji: an emoji pin ignores `color`, so the
  // pinned state would lose its colour affordance.
  function pinIcon() {
    const ns = 'http://www.w3.org/2000/svg'
    const svg = document.createElementNS(ns, 'svg')
    svg.setAttribute('viewBox', '0 0 24 24')
    svg.setAttribute('width', '14')
    svg.setAttribute('height', '14')
    svg.setAttribute('aria-hidden', 'true')
    const path = document.createElementNS(ns, 'path')
    path.setAttribute('d', 'M16 3v2l-1 1v5l3 3v2h-5v5l-1 1-1-1v-5H6v-2l3-3V6L8 5V3z')
    path.setAttribute('fill', 'currentColor')
    svg.append(path)
    return svg
  }

  // Built node by node rather than from a template string: a thread title
  // is text the reader typed, or text the model wrote, and the transcript
  // above is the only place on this page that is allowed to take markup.
  function threadRow(thread) {
    const li = document.createElement('li')
    li.dataset.id = thread.id
    if (thread.current) li.classList.add('current')

    const label = threadLabel(thread)
    const title = document.createElement('button')
    title.type = 'button'
    title.className = label.unnamed ? 'title unnamed' : 'title'
    title.textContent = label.text
    // The row ellipsises a long name, so the full one has to be reachable.
    title.title = label.text
    title.addEventListener('click', () => openThread(thread.id))
    li.append(title)

    if (thread.pinned) {
      const pin = document.createElement('span')
      pin.className = 'pin'
      pin.append(pinIcon())
      li.append(pin)
    }

    const when = whenLabel(thread)
    const whenEl = document.createElement('span')
    whenEl.className = when.expiring ? 'when expiring' : 'when'
    whenEl.textContent = when.text
    li.append(whenEl)

    const tools = document.createElement('span')
    tools.className = 'tools'
    tools.append(
      toolButton(pinIcon(), thread.pinned ? 'Unpin' : 'Pin', () => pinThread(thread), thread.pinned),
      toolButton('✎', 'Rename', () => renameInline(li, thread)),
      toolButton('✦', 'Ask Scout for a name', () => suggestTitle(thread)),
      toolButton('✕', 'Delete', () => deleteThread(thread)),
    )
    li.append(tools)
    return li
  }

  function toolButton(glyph, label, onClick, pressed) {
    const b = document.createElement('button')
    b.type = 'button'
    if (typeof glyph === 'string') b.textContent = glyph
    else b.append(glyph)
    b.title = label
    // The glyph is decoration; the label is the only name a screen reader
    // or a hovering cursor can read.
    b.setAttribute('aria-label', label)
    if (pressed !== undefined) b.setAttribute('aria-pressed', String(pressed))
    b.addEventListener('click', (e) => {
      // The whole row opens a thread. Acting on a row must not also switch
      // to it — least of all delete, which would open what it just removed.
      e.stopPropagation()
      onClick()
    })
    return b
  }

  function closeDrawer() {
    sideEl.classList.remove('open')
    menuButton.setAttribute('aria-expanded', 'false')
  }

  // A 404 from any thread route means the thread went — expired, or
  // deleted on another tab. Refresh the list and show whatever is current.
  async function vanished() {
    showNotice('That thread is gone. Showing the newest one.')
    await refreshThreads()
    const res = await fetch('/chat/history')
    if (res.ok) showTurns(await res.json())
  }

  async function openThread(id) {
    hideNotice()
    const res = await post(`/chat/threads/${id}/open`)
    if (res.status === 404) return vanished()
    if (!res.ok) {
      showNotice('Could not open that thread. Try again.')
      return
    }
    showTurns(await res.json())
    closeDrawer()
    await refreshThreads()
  }

  async function pinThread(thread) {
    const res = await post(`/chat/threads/${thread.id}/pin`, { pinned: !thread.pinned })
    if (res.status === 404) return vanished()
    if (!res.ok) showNotice('Could not change that. Try again.')
    await refreshThreads()
  }

  function renameInline(li, thread) {
    const input = document.createElement('input')
    input.value = thread.title ?? ''
    input.maxLength = 80
    input.setAttribute('aria-label', 'Thread name')
    // The row's button and tools give way to the input; the list is
    // rebuilt from the server afterwards, so nothing here is restored by hand.
    li.replaceChildren(input)
    input.focus()
    input.select()
    // Enter, Escape and blur can all arrive for one rename — Enter moves
    // focus off the input, which blurs it. Without this the row would be
    // rebuilt twice and, worse, saved twice.
    let done = false
    const finish = async (save) => {
      if (done) return
      done = true
      const title = input.value.trim()
      if (save && title && title !== thread.title) {
        const res = await post(`/chat/threads/${thread.id}/rename`, { title })
        if (res.status === 404) return vanished()
        if (!res.ok) showNotice('Could not rename that thread. Try again.')
      }
      await refreshThreads()
    }
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') { e.preventDefault(); finish(true) }
      // Held here rather than let through: on a phone this rename is
      // happening inside the open drawer, and the drawer's own Escape
      // would close it — cancelling the rename and the list with it.
      if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); finish(false) }
    })
    input.addEventListener('blur', () => finish(true))
  }

  async function suggestTitle(thread) {
    hideNotice()
    const res = await post(`/chat/threads/${thread.id}/title`)
    if (res.status === 404) return vanished()
    if (res.status === 429) {
      showNotice('Too many names asked for in a row. Give it a few minutes.')
      return
    }
    if (!res.ok) {
      showNotice('Scout could not think of a name. Try again, or rename it yourself.')
      return
    }
    await refreshThreads()
  }

  async function deleteThread(thread) {
    const name = threadLabel(thread).text
    if (!window.confirm(`Delete "${name}"? This cannot be undone.`)) return
    const res = await post(`/chat/threads/${thread.id}/delete`)
    // A 404 here is the outcome asked for: the thread is already gone.
    if (!res.ok && res.status !== 404) {
      showNotice('Could not delete that thread. Try again.')
      return
    }
    const wasCurrent = thread.id === currentThread
    await refreshThreads()
    if (wasCurrent) {
      const history = await fetch('/chat/history')
      showTurns(history.ok ? await history.json() : [])
    }
  }

  // Returns whether there is now a thread to send into — the composer
  // waits on this, because a message with no thread is a 422.
  async function newThread() {
    hideNotice()
    const res = await post('/chat/threads')
    if (!res.ok) {
      showNotice('Could not start a new thread. Reload to try again.')
      return false
    }
    currentThread = (await res.json()).id
    turnsEl.replaceChildren()
    closeDrawer()
    await refreshThreads()
    return true
  }

  menuButton.addEventListener('click', () => {
    const open = sideEl.classList.toggle('open')
    menuButton.setAttribute('aria-expanded', String(open))
    // A drawer opened by keyboard has to put focus inside it, or Tab
    // carries on into the composer behind it.
    if (open) sideEl.querySelector('button')?.focus()
  })
  // The drawer's ways out: Escape, or a tap anywhere else.
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && sideEl.classList.contains('open')) closeDrawer()
  })
  document.addEventListener('click', (e) => {
    if (sideEl.classList.contains('open') && !sideEl.contains(e.target) && !menuButton.contains(e.target)) closeDrawer()
  })
  // A thread renamed on the phone, or one that aged out while this tab sat
  // in the background, should not still be on screen as it was.
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') refreshThreads()
  })

  // Runs one turn: posts the question, streams `agent` events into the
  // status line and the answer bubble, and stops on the one `end` frame
  // every stream carries. `EventSource` cannot POST, so the stream is
  // parsed by hand off the `fetch` body reader instead.
  async function runMessage(text) {
    let answer = ''
    let thinking = ''
    let answerLi = null
    let sawEnd = false
    // A request the server refused outright never opened a stream, so the
    // "connection dropped" report below — which promises the answer is
    // still being written — would be a lie.
    let refused = false

    function renderAnswer() {
      const wasFollowing = following()
      if (!answerLi) {
        answerLi = turnElement('Scout', '')
        turnsEl.append(answerLi)
      }
      answerLi.innerHTML = render(answer)
      follow(wasFollowing)
    }

    try {
      const res = await fetch('/chat/messages', {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'x-scout-csrf': csrfToken },
        body: sendBody(text, currentThread),
      })
      // Both of these are plain refusals, not streams: the thread went
      // between the page loading and this send, or the page predates the
      // server that now requires a thread. Say which, rather than blaming
      // the connection.
      if (res.status === 404) {
        refused = true
        await vanished()
        return
      }
      if (res.status === 422) {
        refused = true
        showNotice('This page is out of date. Reload to keep going.')
        return
      }
      if (!res.ok || !res.body) {
        refused = true
        showNotice('Scout could not be reached. Reload to try again.')
        return
      }

      const reader = res.body.getReader()
      const decoder = new TextDecoder()
      let buffer = ''

      const handleFrame = (block) => {
        const frame = parseFrame(block)
        if (!frame) return
        if (frame.event === 'agent') {
          const evt = JSON.parse(frame.data)
          if ('Tool' in evt) {
            showStatus(evt.Tool)
          } else if ('Notice' in evt) {
            showStatus(evt.Notice)
          } else if ('Thinking' in evt) {
            thinking = applyUpdate(thinking, evt.Thinking)
            showStatus(thinking)
          } else if ('Answer' in evt) {
            answer = applyUpdate(answer, evt.Answer)
            renderAnswer()
          }
        } else if (frame.event === 'end') {
          sawEnd = true
          const end = JSON.parse(frame.data)
          if (end.status === 'busy') {
            showNotice('Scout is already answering something else. Try again in a moment.')
          } else if (end.status === 'error') {
            showNotice(end.message)
          }
          const finished = finalAnswer(end, answer)
          if (finished !== answer) {
            answer = finished
            if (answer === '' && answerLi) {
              // An empty bubble is not a cleared one. Take the turn off the
              // page rather than leave a blank one behind the notice.
              answerLi.remove()
              answerLi = null
            } else {
              renderAnswer()
            }
          }
        }
      }

      while (true) {
        const { done, value } = await reader.read()
        if (done) break
        buffer += decoder.decode(value, { stream: true })
        let sep
        while ((sep = buffer.indexOf('\n\n')) !== -1) {
          handleFrame(buffer.slice(0, sep))
          buffer = buffer.slice(sep + 2)
        }
      }
      buffer += decoder.decode()
      if (buffer.trim()) handleFrame(buffer)
    } catch {
      // Fall through to the sawEnd check below — a thrown read is exactly
      // the same situation as one that stopped without an `end` frame.
    } finally {
      hideStatus()
      // Not awaited: the first answer is what names a thread, and the row
      // should pick that name up without holding the composer shut for it.
      refreshThreads()
      if (!sawEnd && !refused) {
        // The run continues server-side even though our connection did
        // not, and history is written when it finishes — so this is not
        // an error to apologise for, it's a status to report.
        showNotice(
          'The connection dropped before Scout finished. The answer is still being ' +
            'written and will be saved to history — reload to see it once it lands.',
        )
      }
    }
  }

  let running = false

  askForm.addEventListener('submit', async (e) => {
    e.preventDefault()
    if (running) return
    const text = textEl.value.trim()
    if (!text) return
    hideNotice()

    // Held from here rather than from the run, because the thread below is
    // made across an await: two quick Enters would otherwise start two.
    running = true
    sendButton.disabled = true
    try {
      // An account with no threads at all — a first sign-in — has nothing
      // to name in the body, and the send would be a 422. Make one before
      // the box is cleared and before the bubble is appended: `newThread`
      // empties the transcript, and it would take that bubble with it.
      if (currentThread === null && !(await newThread())) return
      textEl.value = ''
      // A box grown to five lines must shrink back, or it sits tall and
      // empty over the answer it just asked for.
      fitComposer()
      turnsEl.append(turnElement('You', text))
      // Unconditional, unlike the answer: sending is an act that means "show
      // me", so it is not content arriving under a reader who moved away.
      turnsEl.scrollTop = turnsEl.scrollHeight
      await runMessage(text)
    } finally {
      running = false
      sendButton.disabled = false
    }
  })

  if (mirrorButton) {
    mirrorButton.addEventListener('click', async () => {
      // Read the state off the DOM rather than a variable: the button is
      // the only place it lives, and two copies would disagree the first
      // time a request failed.
      const on = mirrorButton.getAttribute('aria-pressed') !== 'true'
      mirrorButton.disabled = true
      try {
        const res = await fetch('/chat/mirror', {
          method: 'POST',
          headers: { 'content-type': 'application/json', 'x-scout-csrf': csrfToken },
          body: JSON.stringify({ on }),
        })
        if (!res.ok) throw new Error('refused')
        mirrorButton.setAttribute('aria-pressed', String(on))
        showNotice(on ? 'This thread is being sent to Telegram.' : 'No longer sending to Telegram.')
      } catch {
        showNotice('Could not change that. Try again.')
      } finally {
        mirrorButton.disabled = false
      }
    })
  }

  // `/chat/threads` rather than `/chat/reset`: the sidebar has to learn the
  // new thread's id, and only the threads route hands it back.
  resetForm.addEventListener('submit', async (e) => {
    e.preventDefault()
    await newThread()
  })

  loadHistory()
}

// Guarded so `node --test` can import the pure functions above without a
// document to wire to.
if (typeof document !== 'undefined') {
  start()
}
