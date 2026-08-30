//! The page, and the one thing about it that changes.

use scout_core::core::Admission;

const TEMPLATE: &str = include_str!("index.html");
const TOKEN: &str = "<!--STATUS-->";

/// The page with its status strip filled in.
///
/// Substitution rather than a template engine: there are two variables on
/// this page, and a dependency to interpolate two strings would be a
/// dependency to keep patched forever.
///
/// `sign_in` is whether the deployment has been given the keys for the
/// signed-in half. It is a parameter rather than a link written into
/// `index.html` because the page is embedded with `include_str!` and
/// served identically everywhere: a hardcoded `/sign-in` would advertise a
/// door that 404s wherever `AuthConfig::from_env` came back `None`.
pub fn render(admission: &Admission, sign_in: bool) -> String {
    TEMPLATE.replace(TOKEN, &status_strip(admission, sign_in))
}

fn status_strip(admission: &Admission, sign_in: bool) -> String {
    // Appended after whichever strip follows rather than written into each
    // of them, so the three branches keep saying only what they are about,
    // and outside the `.gate` box because that box is a flex row and a
    // fourth item in it would land beside the button.
    let door = if sign_in {
        r#"
  <p class="caption"><a href="/sign-in">Already have an account? Sign in</a></p>"#
    } else {
        ""
    };
    let strip = match admission {
        // The join URL needs no escaping: it is the bot's own address from
        // `getMe` plus a round code, and `check_round_name` in
        // `scout-core/src/invites.rs` allows only ASCII letters, digits, `-`
        // and `_`, at most 64 of them. None of those characters can close an
        // attribute or open a tag. If that charset ever widens, this is the
        // line that has to change.
        Admission::Open { join_url: Some(url) } => format!(
            r#"<div class="gate">
    <span class="pill open"><span class="dot"></span>Invites open</span>
    <p>There is room in the current round.</p>
    <a class="btn" href="{url}">Start on Telegram</a>
  </div>"#
        ),
        // No link to give, so no instruction either. `return_url` is read
        // once from `getMe` when the process starts and never retried, so
        // this is not a blink — a bot that could not read its own name keeps
        // this state until someone restarts it. Telling a visitor to go and
        // message a bot we cannot name would be an instruction they have no
        // way to follow, for hours. Say the true thing and offer the link
        // that always works, which is what the full state already does.
        Admission::Open { join_url: None } => r#"<div class="gate">
    <span class="pill open"><span class="dot"></span>Invites open</span>
    <p>There is room in the current round.</p>
    <a class="btn ghost" href="https://github.com/watchcat/scout">Read the source</a>
  </div>"#
            .to_string(),
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
            false,
        );
        assert!(open.contains("https://t.me/scoutbot?start=autumn"));
        assert!(open.contains("Invites open"));
        assert!(!open.contains("<!--STATUS-->"), "the token survived into the page");

        let full = render(&Admission::Full, false);
        assert!(full.contains("Currently full"));
        assert!(!full.contains("t.me"), "a full round must not hand out a join link");
    }

    #[test]
    fn a_round_with_no_known_join_link_still_says_it_is_open() {
        // return_url is None when getMe failed at start-up. Saying "open"
        // with no button is honest; inventing a link is not.
        let page = render(&Admission::Open { join_url: None }, false);
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
        let configured = render(&Admission::Full, true);
        assert!(configured.contains(r#"href="/sign-in""#), "no way in from the front page");

        let bare = render(&Admission::Full, false);
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
        let page = render(&Admission::Full, false);
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
            let page = render(&a, false);
            assert!(!page.contains("seats"), "{a:?} leaked capacity");
            assert!(!page.contains("remaining"), "{a:?} leaked capacity");
        }
    }
}
