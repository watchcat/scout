//! The routes that exist only when the deployment has been given keys.
//!
//! Kept apart from `lib.rs` so that "what the public sees" and "what a
//! signed-in visitor sees" are two files rather than two halves of one, and
//! so the mount point in `router` is a single line that is either there or
//! is not.

pub mod auth;
