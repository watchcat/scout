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
//! title, its address, and its text, less what the page hides in the usual
//! ways, with the things that can be pressed numbered. The rules are
//! against a model led astray by a page:
//!
//! - one shop: the main frame stays on the registrable domain the session
//!   was opened on (`site.rs`), and off its checkout and its sign-in
//!   (`address.rs`);
//! - nothing personal typed: a field is judged by what the page says of
//!   it, and what is typed by its shape, before anything goes in
//!   (`typing.rs`), and it goes into the field that was judged (`tab.rs`);
//! - no side doors: nothing downloaded, nothing uploaded, no permission
//!   given, no second window (`tab.rs`), no datagram sent and nothing
//!   fetched ahead of a press (`chrome.rs`);
//! - a number means what it meant: a press lands on the thing numbered or
//!   is not made, and says what it pressed (`tab.rs`, `in_page.js`);
//! - bounded: three at once, 25 actions, 180 seconds (`limits.rs`), over at
//!   a refusal (`wall.rs`), nothing done for a session that is over
//!   (`session.rs`), and nothing of a page's writing handed on at a length
//!   of the page's choosing (`drive.rs`).
//!
//! The numbers and several of the rules come from a reading of twenty
//! sessions on ten shops, `docs/2026-11-18-shop-reach.md`, made with a
//! prototype, `scripts/shop-reach.mjs`. Where the spec and the reading
//! disagree the reading is later and is followed: the time is 180 seconds
//! and not 120, a session ends itself at a refusal, and a checkout or a
//! sign-in is an address refused and not a sentence in a brief.
//!
//! The calls are in `server.rs`. To try it, with a Chrome on the machine
//! (`SCOUT_CHROME` is the whole path of one that is not where a package
//! puts it):
//!
//! ```text
//! cargo run -p scout-browser        # listens on 127.0.0.1:8091, and only on loopback
//! curl -d '{"url": "https://www.shop.example/p/widget"}' -H 'content-type: application/json' 127.0.0.1:8091/sessions
//! ```
//!
//! # The tests and a machine with no Chrome
//!
//! The tests of the rules themselves need no browser. The rest drive a real
//! Chrome on pages the test serves itself. On a machine with none, each of
//! those writes `SKIPPED, NOT RUN` and its name to stderr and passes, so
//! the run is green and has not tested the browser. Where that must not
//! pass for a pass, set `SCOUT_REQUIRE_CHROME` to anything: every test that
//! wanted a Chrome then fails for the want of one.
//!
//! # Before anything calls this
//!
//! What is known to be missing or untried, for whoever puts this in a pod
//! or in front of a model. None of it is a rule above failing; all of it
//! is outside what this crate was built to be so far.
//!
//! - **Nothing stands in front of it.** No sign-in, no check of the `Host`
//!   a call names, and so `main.rs` refuses to listen anywhere but
//!   loopback. The pod and its network policy are to be the fence.
//! - **Only Chrome 157 on macOS has run it.** Not the image's Chromium, and
//!   not in a container: the flags a container needs (`--no-sandbox`,
//!   `--disable-dev-shm-usage`) are not passed, and the two preferences in
//!   `chrome.rs` were measured on that one Chrome.
//! - **No session has opened a public page.** Every test serves its own on
//!   loopback, past the proxy by a rule only a test can set. That the proxy
//!   lets a public page through is known from the render, which starts
//!   Chrome with the same two flags, and not from here.
//! - **It stops on Ctrl-C and on nothing else.** A `SIGTERM`, which is what
//!   a pod is sent, ends the process without closing its sessions, and
//!   nothing sweeps the profiles a killed server leaves in the temporary
//!   directory. Its Chromes are then left to end themselves on finding
//!   their pipe closed: one idle Chrome whose holder was killed outright
//!   was gone 0.1, 0.4 and 2.7 seconds later in three tries (2026-11-20,
//!   Chrome 157, with a stand-in for the server holding the pipe). A
//!   Chrome in the middle of a page was not tried.
//! - **Calls that wait for a place are not counted.** Three sessions run at
//!   once; any number of calls may wait, each for up to its 180 seconds.
//! - **Letting go of a page can hold a worker for up to three seconds.** A
//!   page dropped unclosed waits, on the thread it is dropped on, for its
//!   Chrome to die (`pipe.rs`, given up after one second) and then for its
//!   profile to stay removed (`chrome.rs`, given up after two). A profile
//!   that is gone at the first try is watched for a tenth of a second.
//! - **What is in a frame is not read**, as in the prototype, so a consent
//!   notice that a shop shows in one cannot be put away. Nothing that only
//!   wraps a frame is numbered.
//! - **Enter is pressed in a second call**, after the caret is looked at
//!   again; a page can still move the caret between the two, and what goes
//!   there then is Enter and nothing typed.
//! - **The lists of words are guesses.** What a shop calls its checkout, its
//!   sign-in and a person's own fields is two lists in three languages, a
//!   first one and what a review found it let through. What is typed is
//!   judged by its shape as well, which does not depend on them.
//! - **Chrome talks to its maker.** A fresh profile asked a proxy for seven
//!   of Google's hosts in twenty seconds (measured 2026-11-20, Chrome 157),
//!   and the five flags tried against that removed two of them.

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
