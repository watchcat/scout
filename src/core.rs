use crate::agent::AgentDeps;
use crate::config::Config;
use std::collections::HashSet;

/// Everything answering a question needs, and nothing about how the question
/// arrived.
///
/// In phase 2b-2 this is what the HTTP service owns. Nothing here may learn
/// what a `ChatId` is.
pub struct Core {
    pub cfg: Config,
    pub deps: AgentDeps,
}

/// Founders are the people paying for the bot, named in the environment by
/// Telegram id because that is the only name they had when the list was
/// written. Exempt from the daily cap.
pub fn is_founder(founders: &HashSet<i64>, telegram_id: i64) -> bool {
    founders.contains(&telegram_id)
}

/// Admin rights are likewise granted by Telegram id in the environment.
pub fn is_admin_id(admins: &HashSet<i64>, telegram_id: i64) -> bool {
    admins.contains(&telegram_id)
}

impl Core {
    pub fn is_founder(&self, telegram_id: i64) -> bool {
        is_founder(&self.cfg.allowed_user_ids, telegram_id)
    }

    pub fn is_admin(&self, telegram_id: i64) -> bool {
        is_admin_id(&self.cfg.admin_user_ids, telegram_id)
    }

    pub fn store(&self) -> crate::store::Store {
        self.deps.store.clone()
    }
}

/// Runs a blocking store call off the async runtime.
///
/// Shared by every module that touches DuckDB: the connection sits behind a
/// mutex, so holding it across an await would block the runtime rather than
/// just the caller.
pub(crate) async fn blocking<T, F>(f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_founder_is_exempt_from_the_daily_cap_and_a_member_is_not() {
        // Authorization is core's business: the adapter's gate is a cache in
        // front of this, never the decision itself.
        let founders: HashSet<i64> = [11_i64].into_iter().collect();
        assert!(is_founder(&founders, 11));
        assert!(!is_founder(&founders, 22));
    }

    #[test]
    fn an_admin_is_named_by_telegram_id_because_that_is_what_env_holds() {
        let admins: HashSet<i64> = [99_i64].into_iter().collect();
        assert!(is_admin_id(&admins, 99));
        assert!(!is_admin_id(&admins, 11));
    }
}
