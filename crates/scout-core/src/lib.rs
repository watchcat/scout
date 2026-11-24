//! Everything that answers a question, and nothing about who asked it.
//!
//! `agent`, `store` and `tools` are private because a channel that can name
//! `Store` can query it, and the boundary goes back to being a convention.
//! What a channel may reach is what `Core` hands it.

mod agent;
mod flights;
mod observer;
mod specialist;
mod store;
mod tools;
mod watch;

pub mod backup;
pub mod config;
pub mod core;
pub mod debug;
pub mod describe;
pub mod geo;
pub mod identity;
pub mod inbox;
pub mod ids;
pub mod invites;
pub mod links;
pub mod measure;
pub mod metrics;
pub mod mirror;
pub mod net_guard;
pub mod move_check;
pub mod nearby;
pub mod retry;
pub mod run;
mod schedule;
pub mod session;
pub mod sharing;
pub mod stats;
pub mod text;
pub mod toolcall;
pub mod trips;
pub mod vision;

/// The renderer, and nothing else of `tools`.
///
/// `tools` stays private because a channel that can name `Store` can query
/// it. The browser pod is not a channel: it holds no account and no
/// secret, and all it does for Scout is turn an address into a rendered
/// page. This is the whole of what crosses that line.
pub mod render {
    pub use crate::tools::browser::{find_chrome, BrowserError, Renderer, RENDER_TIMEOUT};
}
