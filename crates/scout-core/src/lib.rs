//! Everything that answers a question, and nothing about who asked it.
//!
//! `store` and `agent` become private in Task 5, once nothing outside this
//! crate names them. A channel that can name `Store` can query it, and the
//! boundary goes back to being a convention.

pub mod agent;
pub mod store;

pub mod config;
pub mod core;
pub mod describe;
pub mod draft;
pub mod invites;
pub mod links;
pub mod run;
pub mod session;
pub mod stats;
pub mod text;
pub mod tools;
pub mod vision;
