//! Browser sessions: a shop's page driven over HTTP, under rules the caller
//! cannot bend.
//!
//! Nothing uses this crate yet. It is the session half of the `scout-browser`
//! the shop-browsing spec describes
//! (`docs/superpowers/specs/2026-11-17-price-watch-and-shop-browsing-design.md`),
//! built and tested to run on one machine. It is not deployed, it has no
//! `POST /render`, and nothing in `scout-core` calls it: those are the cards
//! "Chromium in a pod of its own" and "The shop desk".

pub mod address;
pub mod chrome;
pub mod drive;
pub mod limits;
pub mod pipe;
pub mod site;
pub mod tab;
pub mod typing;
pub mod wall;

#[cfg(test)]
mod testing;
