//! The page, and the one thing about it that changes.

use scout_core::core::Admission;

const TEMPLATE: &str = include_str!("index.html");
const TOKEN: &str = "<!--STATUS-->";

/// The page with its status strip filled in.
///
/// Substitution rather than a template engine: there is exactly one variable
/// on this page, and a dependency to interpolate one string would be a
/// dependency to keep patched forever.
pub fn render(admission: &Admission) -> String {
    TEMPLATE.replace(TOKEN, &status_strip(admission))
}

fn status_strip(admission: &Admission) -> String {
    match admission {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_open_round_offers_a_way_in_and_a_full_one_offers_none() {
        let open = render(&Admission::Open {
            join_url: Some("https://t.me/scoutbot?start=autumn".to_string()),
        });
        assert!(open.contains("https://t.me/scoutbot?start=autumn"));
        assert!(open.contains("Invites open"));
        assert!(!open.contains("<!--STATUS-->"), "the token survived into the page");

        let full = render(&Admission::Full);
        assert!(full.contains("Currently full"));
        assert!(!full.contains("t.me"), "a full round must not hand out a join link");
    }

    #[test]
    fn a_round_with_no_known_join_link_still_says_it_is_open() {
        // return_url is None when getMe failed at start-up. Saying "open"
        // with no button is honest; inventing a link is not.
        let page = render(&Admission::Open { join_url: None });
        assert!(page.contains("Invites open"));
        assert!(!page.contains("t.me"));
        assert!(
            !page.contains("Message the bot"),
            "an instruction nobody can follow is worse than no instruction"
        );
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
        for class in ["gate", "pill", "open", "full", "dot", "btn", "ghost"] {
            assert!(
                TEMPLATE.contains(&format!(".{class}")),
                "the strip emits class `{class}` and the page has no rule for it"
            );
        }
    }

    #[test]
    fn the_page_never_says_how_many_seats_are_left() {
        // The design says state, not numbers. This is the test that keeps
        // someone from "helpfully" adding a count later.
        for a in [Admission::Full, Admission::Open { join_url: None }] {
            let page = render(&a);
            assert!(!page.contains("seats"), "{a:?} leaked capacity");
            assert!(!page.contains("remaining"), "{a:?} leaked capacity");
        }
    }
}
