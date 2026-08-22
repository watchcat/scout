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
        Admission::Open { join_url: None } => r#"<div class="gate">
    <span class="pill open"><span class="dot"></span>Invites open</span>
    <p>There is room in the current round. Message the bot on Telegram to start.</p>
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
