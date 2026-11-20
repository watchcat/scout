//! What a session asks of a browser, and what it is told back.
//!
//! A trait and not the Chrome itself, so that what bounds and ends a
//! session (`session.rs`) is tested with a page that answers from a script,
//! at any speed and on a machine with no browser. The Chrome behind it is
//! `tab.rs`.

use crate::site::Shop;
use std::future::Future;
use url::Url;

/// How much of a page's visible text one outline carries. The spec's
/// number, and the prototype's.
pub const OUTLINE_CHARS: usize = 6000;

/// What a session does to the page it has open.
///
/// As JSON, an object that names the action under `do`:
/// `{"do": "click", "n": 3}`, `{"do": "type", "n": 4, "text": "1011 PN"}`,
/// `{"do": "back"}`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "do", rename_all = "lowercase", deny_unknown_fields)]
pub enum Action {
    /// The page again; with `more`, the next part of a long one.
    Read {
        #[serde(default)]
        more: bool,
    },
    /// Press the thing numbered `n`.
    Click { n: u32 },
    /// Type into the field numbered `n`, and press Enter after if asked.
    Type {
        n: u32,
        text: String,
        #[serde(default)]
        enter: bool,
    },
    /// Choose an option in the list numbered `n`.
    Select { n: u32, option: String },
    /// Go back one page.
    Back,
    /// Open another address of the same shop.
    Open { url: String },
}

/// A page as a session is shown it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Seen {
    pub title: String,
    pub url: String,
    /// The visible text with the things that can be pressed numbered, at
    /// most [`OUTLINE_CHARS`] of it.
    pub text: String,
    /// Whether the page goes on after this part.
    pub more: bool,
    /// The HTTP status the page's document came with, where known.
    pub status: Option<u16>,
    /// The page's markup, for judging whether it is a refusal. Not shown.
    pub html: String,
}

/// What one action came to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Done {
    /// What did not happen, and why: the action itself, or something it
    /// set off. Empty when everything asked for was done.
    pub refused: Vec<String>,
    /// What else happened that the page does not show.
    pub notes: Vec<String>,
    /// The page afterwards.
    pub seen: Seen,
}

/// Why there is no page to show.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Broken {
    /// A rule stood in the way of opening the page at all.
    #[error("{0}")]
    Refused(String),
    /// The page did not load.
    #[error("the page could not be opened: {0}")]
    Unreachable(String),
    /// The browser could not be started, went away or stopped answering.
    #[error("the browser failed: {0}")]
    Failed(String),
}

/// One page of one shop, in a browser of its own.
pub trait Page: Send + 'static {
    /// Does `action`, within the rules, and says what the page is after.
    fn act(&mut self, action: &Action) -> impl Future<Output = Result<Done, Broken>> + Send;

    /// Ends the browser and everything it held.
    fn close(self) -> impl Future<Output = ()> + Send;
}

/// Where pages come from.
pub trait Browser: Send + Sync + 'static {
    type Page: Page;

    /// Opens `url` in a fresh browser held to `shop`.
    fn open(&self, shop: &Shop, url: &Url) -> impl Future<Output = Result<(Self::Page, Done), Broken>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asked(json: &str) -> Result<Action, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
    }

    #[test]
    fn each_action_is_an_object_that_names_itself() {
        assert_eq!(asked(r#"{"do": "read"}"#), Ok(Action::Read { more: false }));
        assert_eq!(asked(r#"{"do": "read", "more": true}"#), Ok(Action::Read { more: true }));
        assert_eq!(asked(r#"{"do": "click", "n": 3}"#), Ok(Action::Click { n: 3 }));
        assert_eq!(asked(r#"{"do": "type", "n": 4, "text": "1011 PN"}"#), Ok(Action::Type { n: 4, text: "1011 PN".into(), enter: false }));
        assert_eq!(asked(r#"{"do": "type", "n": 1, "text": "kattenvoer", "enter": true}"#), Ok(Action::Type { n: 1, text: "kattenvoer".into(), enter: true }));
        assert_eq!(asked(r#"{"do": "select", "n": 5, "option": "XL"}"#), Ok(Action::Select { n: 5, option: "XL".into() }));
        assert_eq!(asked(r#"{"do": "back"}"#), Ok(Action::Back));
        assert_eq!(asked(r#"{"do": "open", "url": "https://www.shop.example/delivery"}"#), Ok(Action::Open { url: "https://www.shop.example/delivery".into() }));
    }

    #[test]
    fn an_action_that_is_not_one_of_the_six_or_is_missing_a_part_is_not_guessed_at() {
        for json in [
            r#"{"do": "buy"}"#,
            r#"{"do": "evaluate", "script": "1"}"#,
            r#"{"do": "upload", "n": 1, "path": "/etc/passwd"}"#,
            r#"{"n": 3}"#,
            r#"{"do": "click"}"#,
            r#"{"do": "click", "n": -1}"#,
            r#"{"do": "click", "n": "3"}"#,
            r#"{"do": "type", "n": 4}"#,
            // A part an action does not have is a mistake, not something to drop.
            r#"{"do": "type", "n": 4, "text": "x", "entr": true}"#,
            r#"{"do": "read", "from": 6000}"#,
            r#""read""#,
        ] {
            assert!(asked(json).is_err(), "{json}");
        }
        // Going back has no parts, and nothing said beside it is read: it
        // goes back one page, wherever it was told to go.
        assert_eq!(asked(r#"{"do": "back", "to": "https://other.example/"}"#), Ok(Action::Back));
    }
}
