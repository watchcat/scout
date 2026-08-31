# A Landing Page With A System — Design

## Purpose

The landing page reads as plain, and the reason is measurable rather than
aesthetic. It carries **twelve distinct font sizes** — 11.5, 13, 13.5, 14,
14.5, 15, 16.5, 19, 26, 32, 33, 44 — six of them within three and a half
pixels of each other. It carries **nineteen distinct spacing values**. It has
**no focus states at all**, on a page whose whole job is to get someone to
press one button.

Plain is almost never the absence of ornament. It is the absence of system:
nothing here is ugly, but 14px beside 14.5px is a wobble the eye registers
and cannot name, and a reader who cannot name it concludes the page was not
cared about. Everything else on this page *was* cared about — the copy is
specific, the price board is real, the bug list is honest. The measurements
are the part that drifted.

This changes the surface only. No copy moves, no section is reordered, no
class is renamed.

## Not building

Stated first, because this is a page that would be easy to wreck.

- **No copy changes.** "€640 for a €6.48 bottle of cola. The shop published
  the price twice — once in euros, once in cents" is worth more than any
  amount of styling, and rewriting it is a different project with a
  different brief.
- **No reordering.** Hook → proof → gate → depth is already right: the
  reader meets the call to action immediately after the one object that
  proves the claim.
- **No new class names.** `page.rs` asserts on `gate`, `pill`, `open`,
  `full`, `dot`, `btn`, `ghost` and `caption`. Keeping them means those
  tests keep working, and they are the only thing standing between a
  restyle and a broken sign-up button.

## A type scale

Six steps, and every size on the page maps onto one of them:

| Token | Size | Used by |
|---|---|---|
| `--t-xs` | 13px | `h2` labels, `.caption`, `.tag`, `.board`, footer |
| `--t-sm` | 15px | section body, `.gate p`, `.btn`, `.src` copy |
| `--t-base` | 16px | body, `h3` |
| `--t-lede` | 19px | `.lede` |
| `--t-word` | 32px | the wordmark |
| `--t-display` | 44px | `h1` |

`h3` drops from 16.5px to `--t-base` and keeps its 650 weight. **Weight, not
size, is the right lever for a subhead inside a column** — half a pixel of
size difference is noise, where a weight change is legible at a glance.

`h1` and the wordmark scale continuously instead of snapping at 680px — but
**the `clamp()` goes inside the token**, not at the use site:

```css
--t-display: clamp(33px, 5.5vw, 44px);
--t-word:    clamp(26px, 3.4vw, 32px);
```

so every `font-size` on the page is still a bare `var(--t-…)` and the
one-declaration-per-token test below stays true. Writing the clamp at the
use site would make `h1` the one exception to the system, which is the
failure this whole change exists to remove.

The existing media query stays for the grid collapses, which are genuinely a
layout change and not a size one.

## A spacing rhythm

Nineteen values collapse onto a 4px base:

`--s-1: 4px`, `--s-2: 8px`, `--s-3: 12px`, `--s-4: 16px`, `--s-5: 24px`,
`--s-6: 32px`, `--s-7: 48px`, `--s-8: 72px`.

**Sections stop being uniform.** Today every one is `padding: 30px 0`, which
gives a seven-section page the pacing of a list. The hero keeps `--s-8`
below it; ordinary sections take `--s-6`; the gap between a heading and its
own content tightens to `--s-4`. Grouping by proximity is what lets a long
page be skimmed — related things sit close, unrelated things do not — and it
costs nothing but consistency.

## The board becomes an object

The price board is the most persuasive thing on the page and is currently
styled as a code block: monospace on a grey slab, `line-height: 1.9`,
everything the same weight but the winning row.

It stays monospace — **the column alignment is the argument**, and a
proportional font would break the thing it exists to show. What changes is
that it gets treated as a designed artefact rather than a `<pre>`:

- The winning row is marked by more than brightness: a left rule in
  `--green`, which is already the unit colour, so the eye lands on it before
  reading a single number.
- `→ €0.31` gets horizontal room. That arrow is the whole claim — sticker
  price on the left, real price on the right — and it is currently the most
  cramped thing in the block.
- The header row's rule moves from `#0d4a5a` to a token, because a bare hex
  in a file with a full palette is a value nobody can reason about.

## Interaction

From `emil-design-eng`, and each of these earns its place:

- **`:active { transform: scale(0.97) }` on every pressable.** The single
  highest-value change here. A button that does not move when pressed does
  not feel like it heard you.
- **One curve: `--ease-out: cubic-bezier(0.23, 1, 0.32, 1)`.** The built-in
  easings are too weak to read as deliberate.
- **Transitions on the exact properties**, never `all`: `transform 160ms`,
  `background 150ms`, `color 150ms`.
- **Hover gated behind `@media (hover: hover) and (pointer: fine)`.** On a
  touch device hover fires on tap and then *sticks*, so today the Telegram
  button stays lit after being pressed.
- **A real `:focus-visible` ring** — 2px in `--blue` with a 2px offset. The
  page has none, and the primary call to action is a link with
  `text-decoration: none` inside a `border: 0` button.
- **A 40ms stagger** on `.cols` and `.src` children, entering with `opacity`
  and `translateY(8px)`.

That last one needs its justification stated, because the same skill would
forbid it elsewhere in this product. Animate what is seen rarely; leave
alone what is seen constantly. **A landing page is seen once or twice per
visitor; `/chat` is used every day and must stay exactly as still as it is
now.** Same codebase, opposite answers, and the rule is what makes the
difference legible rather than arbitrary.

The stagger uses **`@starting-style`**, not `opacity: 0` plus
`animation-fill-mode: forwards`. The failure modes differ and only one is
acceptable: without `@starting-style` support the content simply appears
un-animated, whereas a fill-mode approach that does not run leaves the page
**blank**. A decorative flourish must not be able to hide the product.

`@media (prefers-reduced-motion: reduce)` drops every transform and keeps
opacity and colour, which is the distinction that matters — motion sickness
comes from movement, not from things changing colour.

## Testing, and what cannot be tested

**Nothing here proves the page looks better.** That needs your eyes, and I
expect to iterate once you have looked. The tests below only stop it
breaking in ways that are invisible from a diff.

- **Every class `page.rs` injects still exists in the stylesheet**, so a
  restyle cannot silently orphan the gate. `every_class_the_strip_emits_is_one_the_page_can_style` in `page.rs`
  already does exactly this, over all eight classes. It needs no change —
  it needs *not to be broken*, which is the point of leaving the names
  alone.
- **The page still carries no inline script.** The CSP has no
  `'unsafe-inline'`, and everything in this change is CSS.
- **One declaration per token**, asserted narrowly enough to be true:
  every `font-size:` carries a `var(--t-`, and every `padding:` and
  `margin:` carries only `var(--s-` or `0`. Deliberately *not* extended to
  `border-radius`, `border-width`, `gap` or the logo's `96px` — those are
  not spacing, and a test that claimed they were would be wrong in a way
  someone would eventually silence rather than fix.

  This is the test that matters most, because it is the only one that
  catches the drift coming back. Twelve sizes did not arrive at once; they
  arrived one reasonable half-pixel at a time.
- **`:focus-visible` exists on the pressables**, asserted from the
  stylesheet the way the chat page's `overflow-wrap` rule is.

## Deferred

- **The prose positioning.** The page is written for someone technical —
  "does the maths in Rust", "the same JSON endpoint marktplaats.nl's own
  front end calls". Whether that is right depends on who Scout is for, which
  is a question about the product and not about the page.
- **The board as live data.** It currently shows one real comparison,
  hard-coded. Making it real would be persuasive and is a feature, not a
  restyle.
- **Anything below the fold on mobile.** The grid collapses are already
  handled; whether the *order* should change on a small screen is a
  structural question this change has explicitly ruled out.
