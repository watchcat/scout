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
export function shouldFollow(scrollTop, clientHeight, scrollHeight, slack = FOLLOW_SLACK) {
  return scrollHeight - scrollTop - clientHeight <= slack
}

// Tallest the composer may grow, in px — about five lines at this font.
// Matches the `max-height` in chat.html; if one changes the other must.
const COMPOSER_CAP = 200

export function composerHeight(scrollHeight, cap = COMPOSER_CAP) {
  return Math.min(scrollHeight, cap)
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

  function turnElement(role, text) {
    const li = document.createElement('li')
    li.className = role === 'You' ? 'you' : 'scout'
    li.innerHTML = render(text)
    return li
  }

  function showStatus(text) {
    statusEl.textContent = text
    statusEl.hidden = false
  }

  function hideStatus() {
    statusEl.hidden = true
    statusEl.textContent = ''
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
    const turns = await res.json()
    for (const turn of turns) {
      turnsEl.append(turnElement(turn.role, turn.text))
    }
  }

  // Splits one SSE block ("event: ...\ndata: ...") into its two fields.
  // Multiple `data:` lines are legal SSE and get joined with `\n`, though
  // this protocol only ever sends one.
  function parseFrame(block) {
    let event = null
    const dataLines = []
    for (const line of block.split('\n')) {
      if (line.startsWith('event:')) event = line.slice('event:'.length).trim()
      else if (line.startsWith('data:')) dataLines.push(line.slice('data:'.length).trim())
    }
    if (!event) return null
    return { event, data: dataLines.join('\n') }
  }

  // Runs one turn: posts the question, streams `agent` events into the
  // status line and the answer bubble, and stops on the one `end` frame
  // every stream carries. `EventSource` cannot POST, so the stream is
  // parsed by hand off the `fetch` body reader instead.
  async function runMessage(text) {
    let answer = ''
    let thinking = ''
    let answerLi = null
    let sawEnd = false

    function renderAnswer() {
      if (!answerLi) {
        answerLi = turnElement('Scout', '')
        turnsEl.append(answerLi)
      }
      answerLi.innerHTML = render(answer)
    }

    try {
      const res = await fetch('/chat/messages', {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'x-scout-csrf': csrfToken },
        body: JSON.stringify({ text }),
      })
      if (!res.ok || !res.body) {
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
      if (!sawEnd) {
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
    textEl.value = ''
    hideNotice()
    turnsEl.append(turnElement('You', text))

    running = true
    sendButton.disabled = true
    try {
      await runMessage(text)
    } finally {
      running = false
      sendButton.disabled = false
    }
  })

  resetForm.addEventListener('submit', async (e) => {
    e.preventDefault()
    hideNotice()
    const res = await fetch('/chat/reset', {
      method: 'POST',
      headers: { 'x-scout-csrf': csrfToken },
    })
    if (res.ok) {
      turnsEl.replaceChildren()
    } else {
      showNotice('Could not start a new thread. Reload to try again.')
    }
  })

  loadHistory()
}

// Guarded so `node --test` can import the pure functions above without a
// document to wire to.
if (typeof document !== 'undefined') {
  start()
}
