//! A per-key request throttle held in memory.
//!
//! In memory rather than in the database for the same reason sessions are:
//! this is consulted on a path a stranger controls, and the database must
//! not be. The worst case of the choice is that a deploy forgets who was
//! being noisy, which is the right shape of failure for a counter.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct Limiter {
    quota: usize,
    window: Duration,
    seen: Mutex<HashMap<String, Vec<Instant>>>,
}

impl Limiter {
    pub fn new(quota: usize, window: Duration) -> Self {
        Self { quota, window, seen: Mutex::new(HashMap::new()) }
    }

    /// True when this key may proceed, counting the attempt.
    pub fn allow(&self, key: &str) -> bool {
        let now = Instant::now();
        // A poisoned lock means another thread panicked mid-update. The
        // stake is a rate counter, so recovering and carrying on beats
        // taking the sign-in page down.
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());

        // Sweep every key, not just this one: otherwise an attacker who
        // never repeats an address grows the map without bound.
        seen.retain(|_, hits| {
            hits.retain(|t| now.duration_since(*t) < self.window);
            !hits.is_empty()
        });

        let hits = seen.entry(key.to_string()).or_default();
        if hits.len() >= self.quota {
            return false;
        }
        hits.push(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_key_is_allowed_its_quota_and_then_refused() {
        let l = Limiter::new(3, Duration::from_secs(900));
        assert!(l.allow("a@example.com"));
        assert!(l.allow("a@example.com"));
        assert!(l.allow("a@example.com"));
        assert!(!l.allow("a@example.com"), "a fourth request in the window was allowed");
        // One noisy address must not lock out everyone else.
        assert!(l.allow("b@example.com"));
    }

    #[test]
    fn the_window_lets_go() {
        let l = Limiter::new(1, Duration::from_millis(50));
        assert!(l.allow("k"));
        assert!(!l.allow("k"));
        std::thread::sleep(Duration::from_millis(60));
        assert!(l.allow("k"), "the window never expired");
    }
}
