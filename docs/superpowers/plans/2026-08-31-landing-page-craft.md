# Landing Page Craft Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give `crates/scout-web/src/index.html` a type scale, a spacing rhythm, focus states and earned motion — without touching a word of copy, the section order, or a single class name.

**Architecture:** One file, one `<style>` block. Twelve font sizes collapse to six tokens, nineteen spacing values to eight, and a test asserts no bare pixel value can creep back in. `page.rs` is not modified at all; its existing `every_class_the_strip_emits_is_one_the_page_can_style` is what proves the gate still has styling.

**Tech Stack:** Hand-written CSS. No build step, no framework, no JavaScript — the CSP forbids inline script and nothing here needs any.

**Spec:** `docs/superpowers/specs/2026-08-31-landing-page-craft.md`

---

## Things to know before starting

**This repository is deliberately not rustfmt-formatted. Never run `cargo fmt`.**

**Clippy needs the rustup toolchain**, or a stale nix clippy shadows it and fails with `E0514`:

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo clippy --workspace --all-targets
```

**Do not rename a class.** `page.rs` injects markup using `gate`, `pill`, `open`, `full`, `dot`, `btn`, `ghost` and `caption`, and asserts on every one of them. They are the only thing between a restyle and a sign-up button that renders unstyled.

**Do not touch the copy or the section order.** If a change seems to require it, stop and say so.

**Two values are deliberately exempt from the scale**, and the tests below are written to allow them:

- `code { font-size: .92em }` — proportional sizing relative to its surrounding text, which is a different thing from a step on a scale. The rule is *no bare pixels*, not *no relative units*.
- `1px` in padding — a hairline, not spacing.

A test that called those violations would be wrong, and a wrong test gets silenced rather than fixed.

---

## Task 1: The test that stops the drift returning

**Files:**
- Modify: `crates/scout-web/src/page.rs` (tests only — no production code)

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/scout-web/src/page.rs`:

```rust
    #[test]
    fn every_size_on_the_page_comes_from_the_scale() {
        // Twelve font sizes and nineteen spacing values did not arrive at
        // once. They arrived one reasonable half-pixel at a time — 14px
        // beside 14.5px, which nobody can see and everybody can feel. The
        // tokens are the fix; this is what stops it happening again.
        //
        // Narrow on purpose: `border-radius`, `border-width`, `gap` and the
        // logo's own `96px` are not spacing, and a test that claimed they
        // were would be wrong in a way someone would eventually silence.
        let css = include_str!("index.html");
        let css = &css[css.find("<style>").expect("styles")..css.find("</style>").expect("styles")];

        for decl in css.split(';') {
            let decl = decl.trim();
            if let Some(value) = decl.strip_prefix("font-size:") {
                assert!(
                    !value.contains("px"),
                    "font-size outside the scale: {decl}\n\
                     every size is a var(--t-…); `em` is fine, a bare px is not"
                );
            }
            for property in ["padding:", "margin:"] {
                let Some(value) = decl.strip_prefix(property) else { continue };
                for token in value.split_whitespace() {
                    assert!(
                        !token.ends_with("px") || token == "1px",
                        "spacing outside the scale: {decl}\n\
                         every value is a var(--s-…), 0, or a 1px hairline"
                    );
                }
            }
        }
    }
```

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p scout-web --lib every_size_on_the_page_comes_from_the_scale
```

Expected: FAIL, naming the first offending declaration (there are dozens). Read the message and confirm it is legible — you will be reading it repeatedly in Task 2.

- [ ] **Step 3: Commit the failing test**

Commit it red, on its own, so the diff that follows is visibly the thing that makes it pass:

```bash
git add crates/scout-web/src/page.rs
git commit -m "test: no size on the landing page outside the scale (currently failing)"
```

Use `--no-verify` only if a hook refuses a failing test; otherwise leave it.

---

## Task 2: The tokens, the type, the spacing

**Files:**
- Modify: `crates/scout-web/src/index.html`

This is one coherent rewrite of the `<style>` block. Piecemeal edits to CSS are how you end up with a fourteenth font size.

- [ ] **Step 1: Add the tokens**

In `:root`, after the existing palette line ending `--green:#859900;`, add:

```css
    /* A six-step type scale. Twelve sizes drifted onto this page one
       reasonable half-pixel at a time. The display steps carry their own
       clamp so every use site stays a bare var() — writing the clamp at the
       use site would make h1 the single exception to the system, which is
       the failure this exists to remove. */
    --t-xs:13px; --t-sm:15px; --t-base:16px; --t-lede:19px;
    --t-word:clamp(26px,3.4vw,32px); --t-display:clamp(33px,5.5vw,44px);
    /* Spacing on a 4px base. */
    --s-1:4px; --s-2:8px; --s-3:12px; --s-4:16px;
    --s-5:24px; --s-6:32px; --s-7:48px; --s-8:72px;
    /* One curve. The built-in easings are too weak to read as deliberate. */
    --ease-out:cubic-bezier(.23,1,.32,1);
    /* Was a bare #0d4a5a in three separate places. */
    --rule:#0d4a5a;
```

- [ ] **Step 2: Convert every rule**

Replace each rule below with its new form. The `font:` shorthand must be split — it hides a size from the test.

```css
  body{
    margin:0; background:var(--base03); color:var(--base0);
    font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Inter,system-ui,sans-serif;
    font-size:var(--t-base); line-height:1.65;
    -webkit-font-smoothing:antialiased;
  }
  .wrap{max-width:880px; margin:0 auto; padding:0 var(--s-5)}
  code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace; font-size:.92em;
    background:var(--base02); padding:1px var(--s-1); border-radius:3px; color:var(--cyan)}

  header{padding:var(--s-8) 0 var(--s-7)}
  .mark{display:flex; align-items:center; gap:var(--s-4); margin-bottom:var(--s-5);
    color:var(--cyan)}
  .mark .logo{width:96px; height:96px; flex:none}
  .mark .word{font-size:var(--t-word); font-weight:700; letter-spacing:-.01em;
    color:var(--base2)}
  h1{font-size:var(--t-display); line-height:1.15; margin:0 0 var(--s-4);
    color:var(--base2); font-weight:700; letter-spacing:-.02em}
  .lede{font-size:var(--t-lede); max-width:37em; margin:0 0 var(--s-6); color:var(--base0)}

  .caption{font-size:var(--t-xs); color:var(--base01); margin:0 0 var(--s-6); max-width:44em}

  .gate{background:var(--base02); border:1px solid var(--rule); border-radius:6px;
    padding:var(--s-5); display:flex; align-items:center; gap:var(--s-5); flex-wrap:wrap}
  .pill{display:inline-flex; align-items:center; gap:var(--s-2); font-size:var(--t-xs);
    font-weight:600; padding:var(--s-1) var(--s-3); border-radius:999px; white-space:nowrap}
  .pill.open{background:rgba(133,153,0,.14); color:var(--green); border:1px solid rgba(133,153,0,.35)}
  .pill.full{background:rgba(203,75,22,.14); color:var(--orange); border:1px solid rgba(203,75,22,.35)}
  .dot{width:7px; height:7px; border-radius:50%; background:currentColor}
  .gate p{margin:0; flex:1; min-width:220px; font-size:var(--t-sm)}
  .btn{background:var(--blue); color:var(--base03); font-weight:650;
    font-size:var(--t-sm); padding:var(--s-3) var(--s-5); border-radius:5px;
    border:0; white-space:nowrap}
  .btn.ghost{background:transparent; color:var(--base1); border:1px solid var(--base01)}

  section{padding:var(--s-6) 0}
  h2{font-size:var(--t-xs); letter-spacing:.14em; text-transform:uppercase;
    color:var(--base01); margin:0 0 var(--s-4); font-weight:600}
  h3{margin:0 0 var(--s-2); font-size:var(--t-base); color:var(--base1); font-weight:650}
  .cols{display:grid; grid-template-columns:repeat(3,1fr); gap:var(--s-5)}
  .cols p{margin:0; font-size:var(--t-sm); color:var(--base00)}

  .chat{background:var(--base02); border-left:3px solid var(--violet);
    padding:var(--s-3) var(--s-4); border-radius:0 4px 4px 0; margin:0 0 var(--s-3);
    font-size:var(--t-sm); color:var(--base1); font-style:italic}

  .src{border-top:1px solid var(--base02); padding:var(--s-4) 0; display:grid;
    grid-template-columns:170px 1fr; gap:var(--s-5); align-items:start}
  .src .who{font-weight:650; color:var(--base1); font-size:var(--t-sm)}
  .src .tag{display:block; font-size:var(--t-xs); font-weight:600; letter-spacing:.08em;
    text-transform:uppercase; margin-top:var(--s-1)}
  .tag.official{color:var(--green)}
  .tag.unofficial{color:var(--yellow)}
  .tag.read{color:var(--cyan)}
  .src p{margin:0; font-size:var(--t-sm); color:var(--base00)}

  table{width:100%; border-collapse:collapse; font-size:var(--t-sm)}
  td{padding:var(--s-3) 0; vertical-align:top; border-top:1px solid var(--base02)}
  td.bad{color:var(--base01); width:50%; padding-right:var(--s-5)}
  td.good{color:var(--base0)}
  .bad b{color:var(--red); font-weight:600}
  .good b{color:var(--green); font-weight:600}

  .aside{background:var(--base02); border-radius:6px; padding:var(--s-4) var(--s-5);
    font-size:var(--t-sm)}
  .aside h3{margin-bottom:var(--s-2)}
  .aside p{margin:0; color:var(--base00)}

  footer{margin-top:var(--s-6); padding:var(--s-5) 0 var(--s-8);
    border-top:1px solid var(--base02); font-size:var(--t-sm);
    color:var(--base01); display:flex; gap:var(--s-5); flex-wrap:wrap}
```

- [ ] **Step 3: Shrink the media query**

`h1` and `.mark .word` now clamp, so their overrides go. What remains is genuinely layout:

```css
  @media(max-width:680px){
    .mark{gap:var(--s-3); margin-bottom:var(--s-4)}
    .mark .logo{width:66px; height:66px}
    .cols{grid-template-columns:1fr; gap:var(--s-4)}
    .src{grid-template-columns:1fr; gap:var(--s-2)}
    td.bad,td.good{display:block; width:auto; padding-right:0}
    td.bad{border-top:1px solid var(--base02); padding-bottom:var(--s-1)}
    td.good{border-top:0; padding-top:0}
  }
```

- [ ] **Step 4: Run the test and watch it pass**

```bash
cargo test -p scout-web --lib every_size_on_the_page_comes_from_the_scale
```

Expected: PASS. If it names a declaration, that rule was missed — fix the rule, not the test.

Then the whole suite, which must be unchanged at 655 plus the one new test:

```bash
cargo test --workspace 2>&1 | grep -E "^test result"
```

- [ ] **Step 5: Verify the gate is still styled**

```bash
cargo test -p scout-web --lib every_class_the_strip_emits_is_one_the_page_can_style
```

Expected: PASS. This is the test that catches a restyle orphaning the sign-up button.

- [ ] **Step 6: Commit**

```bash
git add crates/scout-web/src/index.html
git commit -m "feat: a type scale and a spacing rhythm"
```

---

## Task 3: The board becomes an object

**Files:**
- Modify: `crates/scout-web/src/index.html`

- [ ] **Step 1: Restyle it**

Replace the `.board` rules with:

```css
  /* The most persuasive thing on this page, previously styled as a code
     block. It stays monospace because the column alignment *is* the
     argument — a proportional font would break the thing it exists to
     show. What changes is that it reads as a designed object. */
  .board{
    background:var(--base02); border-radius:6px;
    padding:var(--s-4) var(--s-5); margin:0 0 var(--s-3);
    font-family:ui-monospace,SFMono-Regular,Menlo,monospace;
    font-size:var(--t-xs); overflow-x:auto; line-height:2;
  }
  .board .row{white-space:pre; color:var(--base1); display:block;
    padding-left:var(--s-3); border-left:2px solid transparent}
  /* The winner is marked, not merely brighter: the eye should land on it
     before it reads a single number. `--green` because that is already the
     colour the per-unit figure is written in. */
  .board .row.win{color:var(--base2); border-left-color:var(--green)}
  .board .unit{color:var(--green); font-weight:600}
  .board .ship{color:var(--orange)}
  .board .shop{color:var(--base01)}
  .board .hdr{color:var(--base01); border-bottom:1px solid var(--rule);
    padding-bottom:var(--s-2); margin-bottom:var(--s-2); display:block;
    padding-left:var(--s-3)}
```

`line-height` moves from 1.9 to 2 so the left rule on the winning row has room to read as a rule rather than a tick.

- [ ] **Step 2: Run the tests**

```bash
cargo test -p scout-web --lib 2>&1 | grep -E "^test result"
```

Expected: PASS, including the scale test — every value above is a token.

- [ ] **Step 3: Commit**

```bash
git add crates/scout-web/src/index.html
git commit -m "feat: the price board reads as an object, not a code block"
```

---

## Task 4: Press, hover and focus

**Files:**
- Modify: `crates/scout-web/src/index.html`, `crates/scout-web/src/page.rs` (tests only)

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn the_page_answers_a_press_and_shows_a_keyboard_where_it_is() {
        // The page had no `:active` and no focus styling at all, on a page
        // whose whole job is one button — and that button is a link with
        // `text-decoration:none` inside a `border:0` element, so a keyboard
        // visitor had nothing but the browser default to go on.
        let css = include_str!("index.html");
        let css = &css[css.find("<style>").expect("styles")..css.find("</style>").expect("styles")];
        assert!(css.contains(":active"), "nothing on this page answers a press");
        assert!(css.contains(":focus-visible"), "a keyboard visitor cannot see where they are");
        assert!(
            css.contains("hover: hover"),
            "hover is ungated: on touch it fires on tap and then sticks"
        );
    }
```

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p scout-web --lib the_page_answers_a_press
```

Expected: FAIL on the first assertion.

- [ ] **Step 3: Add the interaction rules**

Replace the bare `a:hover` and `.btn:hover` rules with:

```css
  a{color:var(--blue); text-decoration:none;
    transition:color 150ms var(--ease-out)}
  .btn{transition:transform 160ms var(--ease-out), background 150ms var(--ease-out)}
  /* Every pressable answers a press. A button that does not move when
     pressed does not feel like it heard you. */
  .btn:active{transform:scale(.97)}
  /* A keyboard visitor has to be able to see where they are. The primary
     call to action is a link with no underline inside a borderless button. */
  a:focus-visible,.btn:focus-visible{outline:2px solid var(--blue);
    outline-offset:2px; border-radius:3px}
  /* Gated: on a touch device hover fires on tap and then sticks, so the
     Telegram button stayed lit after being pressed. */
  @media (hover:hover) and (pointer:fine){
    a:hover{text-decoration:underline}
    .btn:hover{text-decoration:none; background:#3196d8}
    .btn.ghost:hover{background:transparent; color:var(--base2)}
  }
```

Delete the original `a:hover{text-decoration:underline}` and `.btn:hover{...}` declarations so each exists exactly once.

- [ ] **Step 4: Run and watch it pass**

```bash
cargo test -p scout-web --lib 2>&1 | grep -E "^test result"
```

- [ ] **Step 5: Mutation-check it**

Delete the `.btn:active` rule, run `cargo test -p scout-web --lib the_page_answers_a_press`, confirm it FAILS. Restore. Then do the same for `:focus-visible` and for the `hover: hover` gate. **Report any that stay green** — a green mutation means the test is decorative.

- [ ] **Step 6: Commit**

```bash
git add crates/scout-web/src/index.html crates/scout-web/src/page.rs
git commit -m "feat: the page answers a press and shows a keyboard where it is"
```

---

## Task 5: Motion, where it is earned

**Files:**
- Modify: `crates/scout-web/src/index.html`

- [ ] **Step 1: Add the stagger**

At the end of the style block, before the media query:

```css
  /* Motion is spent where it is seen rarely and withheld where it is seen
     constantly. A visitor meets this page once or twice; `/chat` is used
     every day and deliberately has none of this.
     
     `@starting-style` rather than `opacity:0` with a forwards animation,
     because the failure modes differ and only one is acceptable: without
     support here the content simply appears un-animated, where a
     fill-mode approach that does not run leaves the page blank. A
     decorative flourish must not be able to hide the product. */
  .cols>div,.src{
    opacity:1; transform:translateY(0);
    transition:opacity 320ms var(--ease-out), transform 320ms var(--ease-out);
  }
  @starting-style{
    .cols>div,.src{opacity:0; transform:translateY(8px)}
  }
  .cols>div:nth-child(2){transition-delay:40ms}
  .cols>div:nth-child(3){transition-delay:80ms}
  .src:nth-of-type(2){transition-delay:40ms}
  .src:nth-of-type(3){transition-delay:80ms}
  .src:nth-of-type(4){transition-delay:120ms}

  /* Movement is what causes motion sickness, not change. Colour and
     opacity stay; every transform goes. */
  @media (prefers-reduced-motion:reduce){
    *{transition-duration:1ms !important; transition-delay:0ms !important}
    .btn:active{transform:none}
    .cols>div,.src{transform:none}
  }
```

- [ ] **Step 2: Check the delays are on the right elements**

`.cols` holds three `<div>` children; `.src` appears four times as a sibling. Confirm with:

```bash
grep -c 'class="src"' crates/scout-web/src/index.html   # expect 4
grep -A1 'class="cols"' crates/scout-web/src/index.html | head -3
```

If `.cols` children are not `div`, adjust the selector to match what is actually there and say so.

- [ ] **Step 3: Run the tests**

```bash
cargo test --workspace 2>&1 | grep -E "^test result"
node --test 'crates/scout-web/src/*.test.mjs' 2>&1 | grep -E "^# (pass|fail)"
```

Expected: all pass. The scale test must still pass — `8px` appears inside a `transform`, not a `padding` or `margin`, so it is outside the rule by design.

- [ ] **Step 4: Commit**

```bash
git add crates/scout-web/src/index.html
git commit -m "feat: motion on the page that is seen once, and none on the one used daily"
```

---

## Task 6: Verification

- [ ] **Step 1: Everything**

```bash
cargo test --workspace 2>&1 | grep -E "^test result"
node --test 'crates/scout-web/src/*.test.mjs'
PATH="$HOME/.cargo/bin:$PATH" cargo clippy --workspace --all-targets 2>&1 | grep -E "^warning|^error"
```

Expected: no failures; clippy silent but for the pre-existing `proc-macro-error2` note.

- [ ] **Step 2: The CSP guard**

```bash
grep -c "<script>" crates/scout-web/src/index.html
```

Expected: `0`. Everything in this change is CSS.

- [ ] **Step 3: Confirm the copy did not move**

```bash
git diff main...HEAD -- crates/scout-web/src/index.html | grep -E "^[-+]" | grep -viE "^[-+][[:space:]]*(/\*|\*|[.#@a-z0-9:>,\[-]+\{|--|[a-z-]+:)" | grep -vE "^[-+]{3}" | head -20
```

Expected: **nothing**. Any line here is prose or markup that changed, which this plan forbids. If something appears, revert that hunk.

- [ ] **Step 4: Report what cannot be checked**

Say plainly in the final summary: nothing in this plan demonstrates the page looks better. It demonstrates that the sizes come from a scale, the gate is still styled, a press is answered, a keyboard visitor can see where they are, and no copy moved. Whether it *reads* as crafted needs eyes on the deployed page.

---

## What this plan does not do

- **The prose positioning.** Deferred in the spec; it is a question about who Scout is for.
- **The board as live data.** A feature, not a restyle.
- **Any reordering on mobile.** The grid collapses are handled; whether the order should change is structural and explicitly out of scope.
