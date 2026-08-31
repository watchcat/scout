//! Ids that are all `i64` and must not be confused.
//!
//! Core is keyed on account ids. A Telegram id is a different number in the
//! same type, and passing one where the other belongs is the mistake that
//! made `/stat` report an empty week. Wrapping the rarer of the two makes
//! that direction a compile error.
//!
//! Account ids stay bare `i64`. Wrapping them too would touch 68 signatures
//! and 595 references and force a `ToSql` impl for every DuckDB bind, for no
//! extra safety: the harmful direction is a Telegram id reaching something
//! that expects an account, and this already blocks it.

/// A Telegram user or chat id, as Telegram issues them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TelegramId(pub i64);

impl std::fmt::Display for TelegramId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
