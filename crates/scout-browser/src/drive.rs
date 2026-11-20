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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// The page again; with `more`, the next part of a long one.
    Read { more: bool },
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
