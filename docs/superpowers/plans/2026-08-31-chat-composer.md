# A Composer That Stays Put — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `/chat` feel like a chat: a composer pinned to the bottom, Enter to send, and new content followed only when the reader is already at the bottom.

**Architecture:** The page becomes a flex column at `100dvh` with one scrolling child, rather than a document that scrolls. Two new pure functions in `chat.js` — a follow-the-bottom predicate and a height cap — carry the only logic worth testing.

**Tech Stack:** Plain HTML and CSS in `crates/scout-web/src/chat.html`, an ES module in `chat.js`, `node --test` for the pure parts.

---

## Read this first

**Nothing in this plan proves the page looks right.** No test will say whether
the composer clears the URL bar on a phone. The tests here catch the two
failures that are otherwise *invisible* — an id the client binds to going
missing, and an inline script the CSP silently refuses — plus the two pure
predicates. Everything else needs eyes on a real device, and the plan says so
rather than implying coverage it does not have.

**The flexbox gotcha that will cost you an hour if you skip it.** A flex child
will not scroll unless it is allowed to be smaller than its content:
`#turns` needs `min-height: 0` as well as `overflow-y: auto`. Without it the
child grows to fit, the body grows with it, and the composer slides off the
bottom exactly as it does today — looking like the change did nothing.

**`100dvh`, not `100vh`.** On mobile `100vh` measures the viewport *without*
browser chrome, so a pinned bottom row lands behind the URL bar. That is the
failure being fixed; using `vh` reintroduces it.

## Verification commands

```bash
cargo test --workspace          # 596 passing, 3 ignored before this plan
node --test 'crates/scout-web/src/*.test.mjs'    # 3 passing before this plan
PATH="/run/current-system/sw/bin:$PATH" cargo clippy --workspace --all-targets
```

Check `cargo-clippy --version` against `rustc --version` first; mismatched
versions produce `E0514` on unrelated crates, which reads like a broken branch
and is not.

**Never run `cargo fmt`.** This repository is deliberately not
rustfmt-formatted.

## File structure

| file | change |
|---|---|
| `crates/scout-web/src/chat.js` | **add** `shouldFollow`, `composerHeight`; wire Enter, auto-grow, follow-on-append |
| `crates/scout-web/src/chat.test.mjs` | **add** tests for the two new functions |
| `crates/scout-web/src/chat.html` | flex-column layout, pinned composer, reset moves to the header |
| `crates/scout-web/src/routes/chat.rs` | **add** a test that the ids the client binds to are all still present |

---

### Task 1: The two things worth testing

**Files:** `crates/scout-web/src/chat.js`, `crates/scout-web/src/chat.test.mjs`

- [ ] **Step 1: Write the failing tests**

Add to `crates/scout-web/src/chat.test.mjs`, and extend the existing import
at the top of that file to include the two new names:

```js
import { applyUpdate, escapeHtml, linkify, shouldFollow, composerHeight } from './chat.js'
```

```js
test('new content is followed only when the reader is already at the bottom', () => {
  // Following on every update drags a reader back down several times a
  // second while they are trying to re-read a long answer — worse than not
  // following at all, and worst exactly when the answer is worth re-reading.
  const atBottom = { scrollTop: 800, clientHeight: 400, scrollHeight: 1200 }
  assert.equal(shouldFollow(atBottom.scrollTop, atBottom.clientHeight, atBottom.scrollHeight), true)

  // A wheel nudge of a line or two still means "I am following along".
  assert.equal(shouldFollow(780, 400, 1200), true)

  // Scrolled up to read. Leave them where they put themselves.
  assert.equal(shouldFollow(300, 400, 1200), false)
})

test('the composer grows with its content and then stops', () => {
  // A box that grows without limit eventually eats the conversation it
  // belongs to.
  assert.equal(composerHeight(40), 40)
  assert.equal(composerHeight(199), 199)
  assert.equal(composerHeight(200), 200)
  assert.equal(composerHeight(4000), 200, 'the cap did not hold')
})
```

- [ ] **Step 2: Run and watch them fail**

```bash
node --test 'crates/scout-web/src/*.test.mjs'
```

Expected: `SyntaxError: Named export 'shouldFollow' not found`. Confirm you
see it.

- [ ] **Step 3: Add the two functions**

In `crates/scout-web/src/chat.js`, beside the other exported helpers:

```js
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
const COMPOSER_CAP = 200

export function composerHeight(scrollHeight, cap = COMPOSER_CAP) {
  return Math.min(scrollHeight, cap)
}
```

- [ ] **Step 4: Run and watch them pass**

```bash
node --test 'crates/scout-web/src/*.test.mjs'
```

Expected: 5 passing.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-web/src/chat.js crates/scout-web/src/chat.test.mjs
git commit -m "feat: the two rules that make a long answer readable"
```

Append these trailers to every commit in this plan:

```
Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01BSFg94PYLWoB4pp2Bd6QzF
```

---

### Task 2: The page stops being a document

**Files:** `crates/scout-web/src/chat.html`, `crates/scout-web/src/routes/chat.rs`

- [ ] **Step 1: Write the failing test**

In `crates/scout-web/src/routes/chat.rs`'s test module:

```rust
    #[tokio::test]
    async fn the_page_still_carries_every_id_the_client_binds_to() {
        // Restyling is exactly the edit that silently unhooks a handler:
        // the JS asks for these by id, and a missing one fails in the
        // browser and nowhere else. Moving the reset control between
        // sections is the specific risk here.
        let (app, core, _dir) = test_app_with_a_round().await;
        let account_id = admitted(&core, "777").await;
        let cookie = crate::session::mint(TEST_KEY, account_id, DAY);
        let page = body_of(get_with_cookie(&app, "/chat", &cookie).await).await;

        for id in ["turns", "status", "notice", "ask", "text", "send", "reset"] {
            assert!(page.contains(&format!(r#"id="{id}""#)), "the client binds to #{id}: {page}");
        }
    }
```

- [ ] **Step 2: Run and watch it pass**

```bash
cargo test -p scout-web the_page_still_carries_every_id
```

This one passes immediately — the ids are all there today. That is the point:
it is a guard for the restyle you are about to do, so land it *before* the
change it protects, and watch it stay green afterwards.

- [ ] **Step 3: Rebuild the layout**

In `crates/scout-web/src/chat.html`, replace the body's `<style>` rules for
`body`, `.wrap`, `.chat`, `.turns`, `.ask`, `textarea`, `button` and `.reset`
with:

```css
  body{margin:0; background:var(--base03); color:var(--base0);
    font:16px/1.65 -apple-system,BlinkMacSystemFont,"Segoe UI",Inter,system-ui,sans-serif;
    -webkit-font-smoothing:antialiased;
    /* dvh, not vh: on mobile `100vh` excludes the browser's own chrome, so
       a pinned bottom row lands underneath the URL bar — the exact failure
       this layout exists to remove. */
    height:100dvh; display:flex; flex-direction:column}
  .wrap{max-width:720px; margin:0 auto; padding:0 24px; width:100%;
    flex:1; display:flex; flex-direction:column; min-height:0}
  header{padding:24px 0 14px; display:flex; align-items:center; gap:14px;
    flex:none; justify-content:space-between}
  .mark{display:flex; align-items:center; gap:12px; color:var(--cyan,#2aa198)}
  .mark .logo{width:32px; height:32px; flex:none}
  .mark .word{font-size:18px; font-weight:700; color:var(--base2)}
  /* `min-height:0` is load-bearing: a flex child will not scroll unless it
     is allowed to be shorter than its content. Without it this grows, the
     body grows with it, and the composer slides off the bottom exactly as
     it did before. */
  .turns{list-style:none; margin:0; padding:4px 0 16px; flex:1; min-height:0;
    overflow-y:auto; display:flex; flex-direction:column; gap:12px}
  .turns li{max-width:82%; padding:12px 16px; border-radius:14px; font-size:15px;
    white-space:pre-wrap; line-height:1.6}
  .turns li.you{align-self:flex-end; background:#1c4f66; color:var(--base2)}
  .turns li.scout{align-self:flex-start; background:var(--base02); color:var(--base1)}
  .status{font-size:13.5px; color:var(--base01); margin:0 0 10px; flex:none}
  .notice{font-size:13.5px; color:var(--red); margin:0 0 10px; flex:none}
  .ask{flex:none; display:flex; align-items:flex-end; gap:8px;
    margin:0 0 20px; padding:8px 8px 8px 16px;
    background:var(--base02); border:1px solid #0d4a5a; border-radius:16px;
    box-shadow:0 8px 28px rgba(0,0,0,.30)}
  .ask textarea{flex:1; border:0; background:transparent; color:var(--base2);
    font:inherit; padding:9px 0; resize:none; overflow-y:auto; max-height:200px}
  .ask textarea:focus{outline:none}
  .ask textarea::placeholder{color:var(--base01)}
  #send{flex:none; width:38px; height:38px; display:flex; align-items:center;
    justify-content:center; background:var(--blue); color:var(--base03);
    border:0; border-radius:11px; cursor:pointer}
  #send:hover{background:#3196d8}
  .reset button{background:transparent; color:var(--base01);
    border:1px solid var(--base01); border-radius:8px; font:inherit;
    font-size:13px; font-weight:600; padding:6px 12px; cursor:pointer}
  .reset button:hover{color:var(--base1)}
```

- [ ] **Step 4: Rebuild the markup**

Replace everything from `<div class="wrap">` to `</div>` with:

```html
<div class="wrap">
<header>
  <a class="mark" href="/">
    <svg class="logo" viewBox="0 0 72 72" aria-hidden="true">
      <g fill="none" stroke="currentColor">
        <circle cx="23.5" cy="36" r="11.5" stroke-width="3.2"/>
        <circle cx="48.5" cy="36" r="11.5" stroke-width="3.2"/>
        <circle cx="23.5" cy="36" r="4" stroke-width="2.6"/>
        <circle cx="48.5" cy="36" r="4" stroke-width="2.6"/>
      </g>
      <rect x="34" y="33.4" width="4" height="5.2" fill="currentColor"/>
    </svg>
    <span class="word">Scout</span>
  </a>
  <form id="reset" class="reset">
    <button type="submit">New thread</button>
  </form>
</header>
<ol id="turns" class="turns"></ol>
<p id="status" class="status" hidden></p>
<p id="notice" class="notice" hidden></p>
<form id="ask" class="ask">
  <textarea id="text" name="text" rows="1"
            placeholder="Ask Scout something" required></textarea>
  <button id="send" type="submit" aria-label="Send">
    <svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true">
      <path d="M12 19V5M5 12l7-7 7 7" fill="none" stroke="currentColor"
            stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/>
    </svg>
  </button>
</form>
</div>
```

Three things changed beyond styling: `<main class="chat">` is gone, because
the flex column is now `.wrap` itself and a second wrapper would need its own
`min-height: 0` to pass the scroll through; the reset form moved into the
header and its label shortened to "New thread"; and the send button's text
became an icon with an `aria-label`, since a button whose only content is an
SVG is unreadable to a screen reader without one.

`rows="1"` because the textarea now grows to fit.

- [ ] **Step 5: Run**

```bash
cargo test --workspace
```

Expected: unchanged from Step 2, and `the_page_still_carries_every_id_the_client_binds_to`
still green. **If that test now fails, an id was lost in the rewrite** — put
it back rather than editing the test.

- [ ] **Step 6: Commit**

```bash
git add crates/scout-web/src/chat.html crates/scout-web/src/routes/chat.rs
git commit -m "feat: the composer stays where you left it"
```

---

### Task 3: Enter sends, the box grows, the page follows

**Files:** `crates/scout-web/src/chat.js`

- [ ] **Step 1: Wire Enter and auto-grow**

`start()` already binds `turnsEl`, `askForm`, `textEl`, `sendButton` and
`resetForm` — reuse them, do not re-declare. Add after those bindings:

```js
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
```

- [ ] **Step 2: Follow the bottom, but only when already there**

Add beside them:

```js
  // Decide *before* appending, act after: once the new content is in the
  // DOM the reader's position looks different and the question cannot be
  // asked honestly any more.
  function following() {
    return shouldFollow(turnsEl.scrollTop, turnsEl.clientHeight, turnsEl.scrollHeight)
  }
  function follow(wasFollowing) {
    if (wasFollowing) turnsEl.scrollTop = turnsEl.scrollHeight
  }
```

Then use them at the three places content is added. In `loadHistory`, wrap
the loop:

```js
    const turns = await res.json()
    for (const turn of turns) {
      turnsEl.append(turnElement(turn.role, turn.text))
    }
    // History always lands at the bottom: it is the state the reader is
    // arriving into, not something arriving under them.
    turnsEl.scrollTop = turnsEl.scrollHeight
```

In `renderAnswer` inside `runMessage`:

```js
    function renderAnswer() {
      const wasFollowing = following()
      if (!answerLi) {
        answerLi = turnElement('Scout', '')
        turnsEl.append(answerLi)
      }
      answerLi.innerHTML = render(answer)
      follow(wasFollowing)
    }
```

- [ ] **Step 3: Send scrolls, and shrinks the box**

The `askForm` submit handler currently reads:

```js
    const text = textEl.value.trim()
    if (!text) return
    textEl.value = ''
    hideNotice()
    turnsEl.append(turnElement('You', text))
```

It becomes:

```js
    const text = textEl.value.trim()
    if (!text) return
    textEl.value = ''
    // A box grown to five lines must shrink back, or it sits tall and
    // empty over the answer it just asked for.
    fitComposer()
    hideNotice()
    turnsEl.append(turnElement('You', text))
    // Unconditional, unlike the answer: sending is an act that means "show
    // me", so it is not content arriving under a reader who moved away.
    turnsEl.scrollTop = turnsEl.scrollHeight
```

- [ ] **Step 4: Run**

```bash
node --test 'crates/scout-web/src/*.test.mjs'
cargo test --workspace
```

Expected: both unchanged — this task adds wiring, not pure logic.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-web/src/chat.js
git commit -m "feat: enter sends, the box fits, the page follows when you are following"
```

---

### Task 4: Verification

- [ ] **Step 1: Everything**

```bash
cargo test --workspace
node --test 'crates/scout-web/src/*.test.mjs'
PATH="/run/current-system/sw/bin:$PATH" cargo clippy --workspace --all-targets
```

- [ ] **Step 2: The CSP guard**

```bash
grep -c "<script>" crates/scout-web/src/chat.html
```

Expected: `0`. The existing test
`a_member_gets_a_page_that_loads_its_script_from_us` already asserts the page
contains no `<script>`, so this grep is belt and braces for the attribute
form it does not cover. An inline `<script>` or an `onclick=` attribute added
while restyling would be refused by the browser's CSP and by nothing else — no test
would fail, the page would simply stop working. Also check:

```bash
grep -cE "on(click|submit|input|keydown)=" crates/scout-web/src/chat.html
```

Expected: `0`.

- [ ] **Step 3: Mutation — the follow rule**

Make `shouldFollow` always return `true`.

```bash
node --test 'crates/scout-web/src/*.test.mjs'
```

Expected: **FAIL** on the scrolled-up case. Revert.

- [ ] **Step 4: Mutation — the height cap**

Make `composerHeight` return `scrollHeight` unchanged.

```bash
node --test 'crates/scout-web/src/*.test.mjs'
```

Expected: **FAIL** on the 4000px case. Revert.

- [ ] **Step 5: Mutation — a lost id**

Remove `id="reset"` from `chat.html`.

```bash
cargo test -p scout-web the_page_still_carries_every_id
```

Expected: **FAIL.** Revert. This is the guard that makes restyling safe.

- [ ] **Step 6: Report**

Give the final counts, the clippy result, the two `grep` results, and the
outcome of each mutation. **A mutation that passes means the test does not
protect what it claims** — report it rather than moving on.

State plainly that the appearance is unverified and needs a human on a real
device, listing what to look at: whether the composer clears the URL bar on a
phone, whether a long answer scrolls under a fixed composer rather than
pushing it away, and whether scrolling up mid-answer leaves the reader where
they put themselves.

---

## What this deliberately does not do

- **A model picker, attachment clip or microphone.** Nothing is behind any of
  them, and a control that does nothing is a promise the page cannot keep.
- **Markdown in answers.** The preamble tells the model to reply in plain
  text; changing that starts with the preamble and would affect Telegram too.
- **A stop button.** Wants a cancel endpoint, and belongs with one.
