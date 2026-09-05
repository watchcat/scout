//! Keeping the gate's set in step with the table.
//!
//! The gate reads an in-memory set on every update — see `App::members`
//! for why it must not read the database there. The set was loaded once at
//! start-up, which was right while the bot was the only thing that could
//! admit anyone. The web can now: a sign-in by email takes a seat, and
//! linking Telegram on the account page puts a Telegram id on it. Until
//! this existed that person was a member in the table and a stranger at
//! the gate until the next deploy, with nothing logged, because a dropped
//! message from a stranger is what the gate is for.

use dashmap::DashSet;
use std::collections::HashSet;
use std::sync::Arc;

/// Where membership is kept, and how to hear that it moved.
///
/// A trait so the watcher can be tested with no database, as
/// `mirror::Sink` is.
pub trait Source {
    /// Resolves once membership has changed since the last time it did.
    async fn changed(&self);
    /// Everyone admitted right now, as Telegram ids.
    async fn current(&self) -> anyhow::Result<Vec<i64>>;
}

impl Source for Arc<scout_core::core::Core> {
    async fn changed(&self) {
        self.membership_changed().await;
    }

    async fn current(&self) -> anyhow::Result<Vec<i64>> {
        // Off the runtime: this takes the store's mutex.
        let core = self.clone();
        tokio::task::spawn_blocking(move || core.members()).await?
    }
}

/// Reloads `members` from `source` every time it says to. Never returns.
///
/// A failed read keeps the set as it was: the gate saying no to someone the
/// table has admitted is a delay until the next change, while an emptied
/// set would turn every member away at once over a database blip.
pub async fn watch(source: impl Source, members: Arc<DashSet<i64>>) {
    loop {
        source.changed().await;
        match source.current().await {
            Ok(current) => {
                replace(&members, current);
                tracing::info!(members = members.len(), "membership reloaded");
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not reload membership; keeping the last set");
            }
        }
    }
}

/// Makes `members` exactly `current` — removals included, because a merge
/// can hand a seat back and a reload that only added would leave the gate
/// open to someone the table has let go.
pub fn replace(members: &DashSet<i64>, current: Vec<i64>) {
    let current: HashSet<i64> = current.into_iter().collect();
    members.retain(|id| current.contains(id));
    for id in current {
        members.insert(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dashmap::DashSet;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::Notify;

    /// Stands in for core: a list that can be changed, and a bell.
    struct Table {
        wake: Notify,
        ids: Mutex<Vec<i64>>,
    }

    impl Source for Arc<Table> {
        async fn changed(&self) {
            self.wake.notified().await;
        }
        async fn current(&self) -> anyhow::Result<Vec<i64>> {
            Ok(self.ids.lock().unwrap().clone())
        }
    }

    async fn eventually(members: &DashSet<i64>, id: i64, present: bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while members.contains(&id) != present {
            assert!(tokio::time::Instant::now() < deadline, "the gate never learned about {id}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn a_member_admitted_elsewhere_reaches_the_gate_without_a_restart() {
        let members = Arc::new(DashSet::new());
        let table = Arc::new(Table { wake: Notify::new(), ids: Mutex::new(vec![]) });
        tokio::spawn(watch(table.clone(), members.clone()));

        *table.ids.lock().unwrap() = vec![888];
        table.wake.notify_one();

        eventually(&members, 888, true).await;
    }

    #[tokio::test]
    async fn a_reload_is_the_table_and_nothing_else() {
        // A merge can hand a seat back, and a reload that only ever added
        // would leave the gate open to someone the table has let go.
        let members: DashSet<i64> = [2, 3].into_iter().collect();

        replace(&members, vec![1, 2]);

        let mut now: Vec<i64> = members.iter().map(|id| *id).collect();
        now.sort();
        assert_eq!(now, vec![1, 2]);
    }

    #[tokio::test]
    async fn a_failed_reload_keeps_the_set_it_had() {
        /// Rings once, then never again — so the loop is exercised exactly
        /// once rather than spun.
        struct Broken(std::sync::atomic::AtomicBool);
        impl Source for Broken {
            async fn changed(&self) {
                if self.0.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    std::future::pending::<()>().await;
                }
            }
            async fn current(&self) -> anyhow::Result<Vec<i64>> {
                anyhow::bail!("the database is away")
            }
        }
        let members: Arc<DashSet<i64>> = Arc::new([7].into_iter().collect());
        let watcher = tokio::spawn(watch(Broken(Default::default()), members.clone()));

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(members.contains(&7), "a blip must not empty the gate");
        watcher.abort();
    }
}
