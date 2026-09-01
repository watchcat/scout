//! The page, and the one thing about it that changes.

use scout_core::core::Admission;

const TEMPLATE: &str = include_str!("index.html");
const TOKEN: &str = "<!--STATUS-->";

/// What the landing page knows about whoever is reading it.
///
/// One bit, and deliberately not two: whether an account is a *member* needs
/// the database, and this page is the one path built to avoid touching it.
/// A queued visitor pressing through is absorbed by `/chat`, which redirects
/// a non-member to `/account` and explains where they stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visitor {
    /// A valid session cookie. Offer the chat.
    SignedIn,
    /// No session, but this deployment has somewhere to sign in.
    SignedOut,
    /// No keys, so no sign-in route exists and no link to it may be shown.
    NoAuth,
}

/// The page with its status strip filled in.
///
/// Substitution rather than a template engine: there are two variables on
/// this page, and a dependency to interpolate two strings would be a
/// dependency to keep patched forever.
///
/// `visitor` is a parameter rather than a link written into `index.html`
/// because the page is embedded with `include_str!` and served identically
/// everywhere: a hardcoded `/sign-in` or `/chat` would advertise a door that
/// 404s wherever `AuthConfig::from_env` came back `None`.
pub fn render(admission: &Admission, visitor: Visitor) -> String {
    TEMPLATE.replace(TOKEN, &status_strip(admission, visitor))
}

fn status_strip(admission: &Admission, visitor: Visitor) -> String {
    // Admission is about getting in, and this visitor is already through.
    // Showing them "Invites open" would be telling them about a queue they
    // have left.
    if let Visitor::SignedIn = visitor {
        return r#"<div class="gate">
    <p>You're signed in.</p>
    <a class="btn" href="/chat">Open Scout</a>
  </div>"#
            .to_string();
    }

    // Appended after whichever strip follows rather than written into each
    // of them, so the three branches keep saying only what they are about,
    // and outside the `.gate` box because that box is a flex row and a
    // fourth item in it would land beside the button.
    let door = match visitor {
        Visitor::SignedIn | Visitor::NoAuth => "",
        Visitor::SignedOut => {
            r#"
  <p class="caption"><a href="/sign-in">Already have an account? Sign in</a></p>"#
        }
    };
    let strip = match admission {
        // The join URL needs no escaping: it is the bot's own address from
        // `getMe` plus a round code, and `check_round_name` in
        // `scout-core/src/invites.rs` allows only ASCII letters, digits, `-`
        // and `_`, at most 64 of them. None of those characters can close an
        // attribute or open a tag. If that charset ever widens, this is the
        // line that has to change.
        Admission::Open { join_url: Some(url) } => {
            let browser = match visitor {
                Visitor::NoAuth => "",
                _ => {
                    r#"<a class="btn" href="/sign-in">Start in your browser</a>
    "#
                }
            };
            format!(
                r#"<div class="gate">
    <span class="pill open"><span class="dot"></span>Invites open</span>
    <p>There is room in the current round.</p>
    {browser}<a class="btn ghost" href="{url}">Start on Telegram</a>
  </div>"#
            )
        }
        // No link to give, so no instruction either. `return_url` is read
        // once from `getMe` when the process starts and never retried, so
        // this is not a blink — a bot that could not read its own name keeps
        // this state until someone restarts it. Telling a visitor to go and
        // message a bot we cannot name would be an instruction they have no
        // way to follow, for hours. Say the true thing and offer the link
        // that always works, which is what the full state already does.
        Admission::Open { join_url: None } => {
            let action = match visitor {
                Visitor::NoAuth => {
                    r#"<a class="btn ghost" href="https://github.com/watchcat/scout">Read the source</a>"#
                }
                _ => r#"<a class="btn" href="/sign-in">Start in your browser</a>"#,
            };
            format!(
                r#"<div class="gate">
    <span class="pill open"><span class="dot"></span>Invites open</span>
    <p>There is room in the current round.</p>
    {action}
  </div>"#
            )
        }
        Admission::Full => r#"<div class="gate">
    <span class="pill full"><span class="dot"></span>Currently full</span>
    <p>This round is closed. Come back soon — new rounds open regularly.</p>
    <a class="btn ghost" href="https://github.com/watchcat/scout">Read the source</a>
  </div>"#
            .to_string(),
    };
    format!("{strip}{door}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_open_round_offers_a_way_in_and_a_full_one_offers_none() {
        let open = render(
            &Admission::Open { join_url: Some("https://t.me/scoutbot?start=autumn".to_string()) },
            Visitor::NoAuth,
        );
        assert!(open.contains("https://t.me/scoutbot?start=autumn"));
        assert!(open.contains("Invites open"));
        assert!(!open.contains("<!--STATUS-->"), "the token survived into the page");

        let full = render(&Admission::Full, Visitor::NoAuth);
        assert!(full.contains("Currently full"));
        assert!(!full.contains("t.me"), "a full round must not hand out a join link");
    }

    #[test]
    fn a_round_with_no_known_join_link_still_says_it_is_open() {
        // return_url is None when getMe failed at start-up. Saying "open"
        // with no button is honest; inventing a link is not.
        let page = render(&Admission::Open { join_url: None }, Visitor::NoAuth);
        assert!(page.contains("Invites open"));
        assert!(!page.contains("t.me"));
        assert!(
            !page.contains("Message the bot"),
            "an instruction nobody can follow is worse than no instruction"
        );
    }

    #[test]
    fn the_page_offers_sign_in_only_where_there_is_a_sign_in() {
        // Nothing linked to `/sign-in` before this, so a visitor could only
        // reach it by knowing the path. The link cannot live in index.html:
        // that file is embedded and served identically everywhere, and on a
        // deployment with no keys the route does not exist — so the link
        // would be an invitation to a 404.
        let configured = render(&Admission::Full, Visitor::SignedOut);
        assert!(configured.contains(r#"href="/sign-in""#), "no way in from the front page");

        let bare = render(&Admission::Full, Visitor::NoAuth);
        assert!(!bare.contains("/sign-in"), "advertised a door that is not there");

        // The strip itself is otherwise untouched, so the two differ by the
        // link and nothing else.
        assert!(bare.contains("Currently full") && configured.contains("Currently full"));
    }

    #[test]
    fn the_template_still_has_somewhere_to_put_the_status() {
        // `replace` on a token that is not there does nothing and says
        // nothing: the page would ship with no status strip at all, no
        // error, no log line. The token is an HTML comment, which reads as
        // inert to anyone editing index.html who has not read this file.
        assert!(TEMPLATE.contains(TOKEN), "index.html has lost its {TOKEN}");
    }

    #[test]
    fn every_class_the_strip_emits_is_one_the_page_can_style() {
        // The strip's markup lives in Rust and the CSS that styles it lives
        // in index.html, with nothing joining them. Renaming a class in the
        // stylesheet would leave the strip unstyled and every other test
        // passing, because they all assert on text and links.
        for class in ["gate", "pill", "open", "full", "dot", "btn", "ghost", "caption"] {
            assert!(
                TEMPLATE.contains(&format!(".{class}")),
                "the strip emits class `{class}` and the page has no rule for it"
            );
        }
    }

    #[test]
    fn the_page_carries_the_mark_and_asks_for_the_icon_that_is_served() {
        // The header's glyph is a copy of icon.svg with the tile dropped and
        // the colour inherited, so the two can drift. These assert the parts
        // that must not: that the page still draws a mark at all, and that
        // the icon path it asks the browser for is the path lib.rs routes.
        let page = render(&Admission::Full, Visitor::NoAuth);
        assert!(page.contains(r#"href="/icon.svg""#), "the page asks for no icon");
        assert!(page.contains(r#"class="logo""#), "the header lost its mark");
        // The two lenses and the bridge between them. If a redraw changes
        // the geometry this fails, which is the point: someone should look.
        assert_eq!(page.matches("<circle").count(), 4, "the mark is not the mark");
    }

    #[test]
    fn the_page_never_says_how_many_seats_are_left() {
        // The design says state, not numbers. This is the test that keeps
        // someone from "helpfully" adding a count later.
        for a in [Admission::Full, Admission::Open { join_url: None }] {
            let page = render(&a, Visitor::NoAuth);
            assert!(!page.contains("seats"), "{a:?} leaked capacity");
            assert!(!page.contains("remaining"), "{a:?} leaked capacity");
        }
    }

    #[test]
    fn someone_signed_in_is_offered_the_chat_rather_than_a_door_they_have_used() {
        let page = render(&Admission::Open { join_url: None }, Visitor::SignedIn);
        assert!(page.contains(r#"href="/chat""#), "no way through to the chat: {page}");
        assert!(
            !page.contains(r#"href="/sign-in""#),
            "offered a sign-in to somebody already signed in"
        );
    }

    #[test]
    fn an_open_round_offers_the_browser_as_well_as_telegram() {
        // The browser door has worked since W2 — `identity::sign_in` claims
        // a seat and the emailed link calls it — and the page has never
        // said so, sending every new visitor to Telegram instead.
        let page = render(
            &Admission::Open { join_url: Some("https://t.me/scoutbot?start=autumn".to_string()) },
            Visitor::SignedOut,
        );
        assert!(page.contains(r#"href="/sign-in""#), "no browser door: {page}");
        assert!(page.contains("https://t.me/scoutbot?start=autumn"), "the Telegram door went");
    }

    #[test]
    fn a_round_with_no_join_link_still_offers_the_browser() {
        // `join_url` is None when getMe failed at start-up, and that state
        // lasts until someone restarts the process. The browser door needs
        // no bot name, so there is no reason to offer nothing for hours.
        let page = render(&Admission::Open { join_url: None }, Visitor::SignedOut);
        assert!(page.contains(r#"href="/sign-in""#));
        assert!(!page.contains("t.me"), "invented a link it does not have");
    }

    #[test]
    fn a_deployment_with_no_keys_offers_no_door_it_cannot_open() {
        // The route does not exist there, so a link to it would be a 404.
        let page = render(&Admission::Full, Visitor::NoAuth);
        assert!(!page.contains(r#"href="/sign-in""#));
        assert!(!page.contains(r#"href="/chat""#));
    }

    #[test]
    fn motion_can_be_turned_off_and_cannot_hide_the_page() {
        // Two separate guarantees, both easy to get wrong in ways that only
        // show up on someone else's machine.
        //
        // `@starting-style` is load-bearing, not stylistic: the obvious
        // alternative — `opacity:0` plus a forwards animation — leaves the
        // page BLANK anywhere the animation does not run, where this
        // degrades to simply appearing.
        //
        // And reduced motion has to override the starting style too. With
        // only the resting rule overridden the content still travels its
        // eight pixels, which is exactly what was asked not to happen.
        let css = include_str!("index.html");
        let css = &css[css.find("<style>").expect("styles")..css.find("</style>").expect("styles")];
        assert!(
            !css.contains("animation-fill-mode"),
            "a fill-mode entrance can leave the page blank; use @starting-style"
        );
        let reduced = css
            .find("prefers-reduced-motion")
            .expect("motion must be refusable");
        assert!(
            css[reduced..].contains("@starting-style"),
            "reduced motion did not override the starting style, so the content still moves"
        );
    }

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
        // Ungated, hover fires on tap on a touch device and then sticks:
        // the Telegram button stayed lit after being pressed.
        assert!(css.contains("hover:hover"), "hover is not gated to devices that have one");
    }

    /// Relative luminance, per WCAG 2.1.
    fn luminance(hex: &str) -> f64 {
        let v = |i: usize| {
            let c = u8::from_str_radix(&hex[i..i + 2], 16).unwrap() as f64 / 255.0;
            if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * v(1) + 0.7152 * v(3) + 0.0722 * v(5)
    }

    fn contrast(a: &str, b: &str) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    /// Every `--name:#rrggbb` declared in the stylesheet.
    fn palette() -> std::collections::HashMap<String, String> {
        let css = include_str!("index.html");
        let mut out = std::collections::HashMap::new();
        for (i, _) in css.match_indices("--") {
            let rest = &css[i + 2..];
            let Some(colon) = rest.find(':') else { continue };
            let name = &rest[..colon];
            if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                continue;
            }
            let value = rest[colon + 1..].trim_start();
            if value.starts_with('#') && value.len() >= 7
                && value[1..7].chars().all(|c| c.is_ascii_hexdigit())
            {
                out.insert(name.to_string(), value[..7].to_string());
            }
        }
        out
    }

    #[test]
    fn no_token_is_used_without_being_defined() {
        // Nearly shipped: a string replace that added `--link` silently
        // matched nothing, so the stylesheet referenced `var(--link)` that
        // did not exist. Every link and the primary call to action would
        // have rendered with no colour, and no test would have said a word.
        let css = include_str!("index.html");
        let css = &css[css.find("<style>").expect("styles")..css.find("</style>").expect("styles")];
        let defined = palette();
        for (i, _) in css.match_indices("var(--") {
            let rest = &css[i + 6..];
            let name = &rest[..rest.find(')').expect("a var() must close")];
            // Sizes and curves are not colours; only colours land in `palette`.
            if name.starts_with("t-") || name.starts_with("s-") || name.starts_with("ease") {
                continue;
            }
            assert!(defined.contains_key(name), "var(--{name}) is used but never defined");
        }
    }

    #[test]
    fn the_text_on_this_page_can_be_read() {
        // Measured, not judged. Ten of fifteen pairs failed WCAG AA before
        // this: a third of the prose, every section label, and — worst —
        // the primary call to action and every link on the page.
        //
        // Solarized's dimmer values are *designed* to recede, which is why
        // they were reached for. WCAG does not grade on intent.
        let p = palette();
        let c = |name: &str| p.get(name).unwrap_or_else(|| panic!("no --{name}")).clone();
        let (bg, surface) = (c("base03"), c("base02"));

        // (what it is, colour, behind, minimum)
        let pairs: &[(&str, String, String, f64)] = &[
            ("body and lede", c("base1"), bg.clone(), 4.5),
            ("prose in columns and sources", c("base0"), bg.clone(), 4.5),
            ("section labels, caption, footer", c("base0"), bg.clone(), 4.5),
            // 44px display: large text, so 3:1 is the bar and the quieter
            // value survives.
            ("the second line of the headline", c("base00"), bg.clone(), 3.0),
            ("anything on a raised surface", c("base1"), surface.clone(), 4.5),
            ("the board's per-unit price", c("unit"), surface.clone(), 4.5),
            ("the board's shipping cost", c("ship"), surface.clone(), 4.5),
            ("link text", c("link"), bg.clone(), 4.5),
            ("the button's label on its fill", bg.clone(), c("link"), 4.5),
            ("the button's label, hovered", bg.clone(), c("link-hover"), 4.5),
        ];
        for (what, fg, back, need) in pairs {
            let got = contrast(fg, back);
            assert!(
                got >= *need,
                "{what}: {fg} on {back} is {got:.2}:1, needs {need}:1"
            );
        }

        // The pairs above prove the palette is sound. They do not prove the
        // rules reach for the right entries — reverting one rule to
        // `base00` left this test green, which is the "passes for a
        // different reason" failure. So: the two values that cannot carry
        // text here must not be used as text.
        let css = include_str!("index.html");
        let css = &css[css.find("<style>").expect("styles")..css.find("</style>").expect("styles")];
        assert!(
            !css.contains("color:var(--base01)"),
            "base01 is 2.79:1 on the page background — it cannot carry text here"
        );
        // base00 is 3.37:1: under AA for body, over the 3:1 bar for large
        // text. The headline's second line is the one place that holds.
        assert_eq!(
            css.matches("color:var(--base00)").count(),
            1,
            "base00 is only legible at display size; the headline is the one place for it"
        );
    }

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

        // Split on `}` as well as `;`. A block whose last declaration has
        // no trailing semicolon would otherwise be glued to the next
        // selector — `margin-bottom:var(--s-4)}\n.mark .logo{width:66px` —
        // and reported as a spacing violation for a width. The first
        // version of this test did that, and the fix applied to it was to
        // add trailing semicolons to two rules: a convention nobody knows
        // about, enforced by a message that names the wrong line.
        for decl in css.split([';', '}']) {
            let decl = decl.trim();
            if let Some(value) = decl.strip_prefix("font-size:") {
                assert!(
                    !value.contains("px"),
                    "font-size outside the scale: {decl}\n\
                     every size is a var(--t-…); `em` is fine, a bare px is not"
                );
            }
            // `padding` and `margin` and every longhand of them:
            // `padding-right` and `margin-top` are both in use today, and
            // matching only the shorthand would let them keep drifting.
            let Some((property, value)) = decl.split_once(':') else { continue };
            if property.starts_with("padding") || property.starts_with("margin") {
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
}
