//! Everything that answers a question, and nothing about who asked it.
//!
//! `agent`, `store` and `tools` are private because a channel that can name
//! `Store` can query it, and the boundary goes back to being a convention.
//! What a channel may reach is what `Core` hands it.

mod agent;
mod store;
mod tools;

pub mod config;
pub mod core;
pub mod describe;
pub mod invites;
pub mod links;
pub mod run;
mod schedule;
pub mod session;
pub mod stats;
pub mod text;
pub mod vision;
