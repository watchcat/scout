# A Composer That Stays Put — Design

## Purpose

The chat works. It does not yet feel like a chat.

The composer sits in the document flow, so it slides off the bottom as soon
as an answer is longer than the viewport. Nothing scrolls to the newest text,
so a streaming answer grows below the fold and has to be chased. Enter puts a
newline in the box rather than sending. Each of those is small; together they
are the difference between a working page and one you would use.

This is a design change to `/chat`. No routes, no protocol, no core.

## Layout: an app, not a document

`body` becomes a flex column at `100dvh`. The transcript takes the remaining
height and scrolls; the composer is the last row and never moves.

`100dvh` and not `100vh`. On mobile, `100vh` is the viewport *without* the
browser's chrome, so a pinned bottom row lands underneath the URL bar —
precisely the failure this change exists to remove. `dvh` tracks the visible
area as that chrome comes and goes.

This is the whole reason the page stops scrolling as a document. A `position:
sticky` composer inside a scrolling page would also stay visible, but the
page would still scroll underneath the header, and on iOS the address bar
collapsing mid-scroll makes a sticky element jump. A flex column with one
scrolling child has neither problem.

## The composer

One rounded surface — 14px radius, a soft border, sitting slightly proud of
the page — containing the textarea and an icon send button. Today's outlined
box beside a wide filled button becomes a single object, which is what makes
it read as a composer rather than a form.

It auto-grows from one line to a cap of **200px** — roughly five lines at this
font — and then scrolls inside itself. A box that grows without limit
eventually eats the conversation it belongs to.

**Enter sends. Shift+Enter is a newline.** The textarea keeps `required`, so
Enter on an empty box does nothing rather than sending an empty message — the
same guard the button already relies on.

"Start a new thread" leaves the composer for a quiet control in the header. A
composer holds one action; a second button beside Send is a chance to destroy
a conversation by aiming badly.

## Scrolling, and the part that is easy to get wrong

New content scrolls into view — **but only when the reader is already at the
bottom.**

The naive version scrolls on every update, which means that scrolling up to
re-read something while an answer streams drags you back down a few times a
second. That is not a small annoyance; it makes the page unusable precisely
when the answer is long enough to be worth re-reading.

So the rule is a predicate: before appending, ask whether the transcript is
scrolled to within **32px** of its bottom. If it is, scroll after appending.
If it is not, leave the reader where they put themselves.

32 rather than 0 because a reader who has nudged the wheel a line or two still
means "I am following along", and a strict comparison would strand them a
pixel from the bottom and never scroll again.

That predicate is pure — `scrollTop`, `clientHeight`, `scrollHeight` and a
tolerance in, a boolean out — so unlike the rest of this document it can be
tested.

## Surfaces

The reader's own messages are currently full `--blue` behind `--base03`,
which is the loudest thing on the page and draws the eye to what they already
know they said. They become a muted blue, still clearly theirs, no longer
shouting.

Scout's messages keep `--base02`, gain padding and a larger radius to match
the composer. Line height stays as it is: 1.65 is already right for reading.

## Not building

The reference's model picker, attachment clip and microphone. There is
nothing behind any of them, and a control that does nothing is worse than an
absent one — it is a promise the page cannot keep.

## Testing, and what cannot be tested

**Nothing here proves the page looks right.** No test will say whether the
composer clears the URL bar on an iPhone. Claiming otherwise would be the
coverage theatre this repository has already rejected once tonight, so it is
stated plainly instead: this change needs eyes on a real device, and the
tests below only stop it breaking in ways that are invisible.

- **The page still carries the ids the client binds to** — `#turns`,
  `#status`, `#notice`, `#ask`, `#text`, `#send`, `#reset` — because moving
  the reset control is exactly the kind of edit that silently unhooks a
  handler.
- **The page still carries no inline script.** The CSP has no
  `'unsafe-inline'`, so an inline handler added while restyling would be
  refused by the browser and by nothing else.
- **Stick to the bottom only when already at the bottom**, over the boundary:
  at the bottom, a pixel above it, and scrolled well up.
- **Auto-grow stops growing.** The computed height rises with content and
  then stops at the cap, rather than tracking it forever.

The last two are pure functions in `chat.js`, exported and covered by
`node --test`, like `applyUpdate` before them.

## Deferred

- **Markdown or rich text in answers.** The agent's preamble tells it to
  reply in plain text; changing that is a change to the preamble first, and
  it would affect Telegram too.
- **A stop button.** Still wants a cancel endpoint, and still belongs with
  one rather than with a restyle.
- **Anything behind the picker, clip or microphone.**
