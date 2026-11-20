//! Browser sessions: a shop's page driven over HTTP, under rules the caller
//! cannot bend.
//!
//! Nothing uses this crate yet. It is the session half of the `scout-browser`
//! the shop-browsing spec describes
//! (`docs/superpowers/specs/2026-11-17-price-watch-and-shop-browsing-design.md`),
//! built and tested to run on one machine. It is not deployed, it has no
//! `POST /render`, and nothing in `scout-core` calls it: those are the cards
//! "Chromium in a pod of its own" and "The shop desk".
//!
//! A session is opened on one page of one shop, in a Chrome of its own
//! behind `scout_core::net_guard`. Six actions (`read`, `click`, `type`,
//! `select`, `back`, `open`) each answer with the page as an outline: its
//! title, its address, and its visible text with the things that can be
//! pressed numbered. The rules are against a model led astray by a page:
//!
//! - one shop: the main frame stays on the registrable domain the session
//!   was opened on (`site.rs`), and off its checkout and its sign-in
//!   (`address.rs`);
//! - nothing personal typed: a field is judged by what the page says of it
//!   before a key is pressed (`typing.rs`);
//! - no side doors: nothing downloaded, nothing uploaded, no permission
//!   given, no second window (`tab.rs`);
//! - bounded: three at once, 25 actions, 180 seconds (`limits.rs`), over at
//!   a refusal (`wall.rs`), and nothing done for a session that is over
//!   (`session.rs`).
//!
//! The numbers and several of the rules come from a reading of twenty
//! sessions on ten shops, `docs/2026-11-18-shop-reach.md`, made with a
//! prototype, `scripts/shop-reach.mjs`. Where the spec and the reading
//! disagree the reading is later and is followed: the time is 180 seconds
//! and not 120, a session ends itself at a refusal, and a checkout or a
//! sign-in is an address refused and not a sentence in a brief.
//!
//! The calls are in `server.rs`. To try it, with a Chrome on the machine
//! (`SCOUT_CHROME` names one that is not where a package puts it):
//!
//! ```text
//! cargo run -p scout-browser        # listens on 127.0.0.1:8091, and only on loopback
//! curl -d '{"url": "https://www.shop.example/p/widget"}' -H 'content-type: application/json' 127.0.0.1:8091/sessions
//! ```

pub mod address;
pub mod chrome;
pub mod drive;
pub mod limits;
pub mod pipe;
pub mod server;
pub mod session;
pub mod site;
pub mod tab;
pub mod typing;
pub mod wall;

#[cfg(test)]
mod testing;
