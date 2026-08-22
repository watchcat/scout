//! What the page says about admission, held in memory.
//!
//! The store is behind one mutex, shared with the agent. A public endpoint
//! that took it on every request would let a stranger slow Scout down with
//! nothing but traffic. So the value is read on a timer and requests read
//! the value.

use scout_core::core::{Admission, Core};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

/// How stale the page may be. Opening a round takes this long to show up,
/// which nobody notices, and it makes every request free.
pub const REFRESH: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct AdmissionCache(Arc<RwLock<Admission>>);

impl AdmissionCache {
    /// The initial value is what the page says until the first successful
    /// read, so a caller whose own first read failed should pass `Full`.
    /// Inviting people into a round nobody has confirmed exists is the worse
    /// of the two available lies.
    pub fn new(initial: Admission) -> Self {
        Self(Arc::new(RwLock::new(initial)))
    }

    /// Never blocks on anything slower than another reader.
    ///
    /// A poisoned lock is read through rather than unwrapped. Poisoning is
    /// permanent, so a single panic near the write guard would otherwise
    /// take the public page down for the life of the process — and the
    /// value it guards is a plain enum that is whole whenever the guard is
    /// not held, so there is nothing here to protect anyone from.
    pub fn get(&self) -> Admission {
        self.0.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn put(&self, next: Admission) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = next;
    }
}

/// One reading, applied. A failed read leaves the last known value in place:
/// the page saying "open" for another thirty seconds is a better failure
/// than the page going blank because DuckDB was briefly busy.
///
/// Separate from the loop below so that this decision — the only judgement
/// in the refresher — can be tested without waiting on a timer.
fn store_or_keep(reading: anyhow::Result<Admission>, cache: &AdmissionCache) {
    match reading {
        Ok(next) => cache.put(next),
        Err(e) => tracing::warn!(error = %e, "could not refresh admission; keeping the last value"),
    }
}

/// Refreshes forever.
///
/// The first tick of a `tokio` interval completes immediately, so this reads
/// once before waiting. That is a duplicate of the reading the caller takes
/// before spawning this, and harmless: it is one extra query at start-up,
/// off the request path, writing the same answer.
pub async fn refresh_forever(core: Arc<Core>, cache: AdmissionCache) {
    let mut ticker = tokio::time::interval(REFRESH);
    // A tokio interval defaults to catching up on ticks it missed, firing
    // them back to back. If a read ever took longer than REFRESH — which
    // happens exactly when the store is already contended — the catch-up
    // would answer a busy database with several more queries at once. This
    // refresher has nothing to catch up on: it wants a recent answer, not
    // every answer, so it waits REFRESH after each read instead.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        store_or_keep(core.admission().await, &cache);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scout_core::core::Admission;

    #[test]
    fn a_read_never_waits_and_a_refresh_is_what_changes_it() {
        // The whole point: a request reads a value. Nothing on the request
        // path can block on the store's mutex, because that mutex is shared
        // with the agent and this endpoint is open to anyone. That the
        // property holds is visible here rather than asserted: this test
        // builds a cache and reads it with no Core, no Store and no database
        // file anywhere, because the cache holds an answer and not a handle
        // to the thing that produced it.
        let cache = AdmissionCache::new(Admission::Full);
        assert_eq!(cache.get(), Admission::Full);

        cache.put(Admission::Open { join_url: Some("https://t.me/x?start=autumn".into()) });
        assert_eq!(
            cache.get(),
            Admission::Open { join_url: Some("https://t.me/x?start=autumn".into()) }
        );
    }

    #[test]
    fn a_database_that_will_not_answer_leaves_the_last_answer_standing() {
        // The failure this module has to get right. A busy or broken store
        // must not empty the page: "open" for another thirty seconds is a
        // small lie, and a blank page is a broken site. The tempting wrong
        // code — `put(read.unwrap_or(Admission::Full))` — closes the door on
        // every reader the moment DuckDB hiccups, and would pass the test
        // above without complaint.
        let cache = AdmissionCache::new(Admission::Open { join_url: None });

        store_or_keep(Err(anyhow::anyhow!("the store is busy")), &cache);
        assert_eq!(cache.get(), Admission::Open { join_url: None });

        store_or_keep(Ok(Admission::Full), &cache);
        assert_eq!(cache.get(), Admission::Full);
    }

    #[test]
    fn the_page_still_answers_after_another_thread_died_holding_the_lock() {
        // Why this is not `.unwrap()` like the locks in scout-core: poisoning
        // is permanent. One panic anywhere near the write guard would turn
        // every later request into a panic, forever, which is the exact
        // outage this module exists to prevent. The value under the lock is
        // whole either way — a write is one move of an `Admission`, so a
        // reader sees the old one or the new one and never a torn one.
        let cache = AdmissionCache::new(Admission::Full);
        let poisoner = cache.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = poisoner.0.write().unwrap();
            panic!("something else went wrong while holding the write guard");
        }));
        assert!(cache.0.is_poisoned(), "the setup did not actually poison the lock");

        assert_eq!(cache.get(), Admission::Full, "a poisoned lock took the page down");
        cache.put(Admission::Open { join_url: None });
        assert_eq!(cache.get(), Admission::Open { join_url: None },
            "a poisoned lock stopped the refresher from ever updating the page");
    }
}
