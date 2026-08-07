use std::sync::atomic::{AtomicUsize, Ordering};

/// Kagi queries allowed per user request, shared by every tool that bills
/// them. One request once spent 24 (eight searches fanned out over three
/// languages), which is real money for one question — and the last of those
/// searches were re-checking pages already opened.
pub const QUERIES_PER_REQUEST: usize = 15;

/// A per-request allowance of billed search queries. Built in `build_agent`
/// and shared between `kagi_search` and `search_secondhand`, so the cap is on
/// what the request costs rather than on how one tool is used.
#[derive(Debug)]
pub struct SearchBudget {
    remaining: AtomicUsize,
}

impl SearchBudget {
    pub fn new(total: usize) -> Self {
        Self { remaining: AtomicUsize::new(total) }
    }

    /// Takes up to `wanted` queries from the allowance and returns how many
    /// were granted — fewer than asked when the budget is nearly spent, 0
    /// when it is gone. Callers narrow their fan-out to what they get.
    pub fn claim(&self, wanted: usize) -> usize {
        let mut left = self.remaining.load(Ordering::Relaxed);
        loop {
            let granted = wanted.min(left);
            if granted == 0 {
                return 0;
            }
            match self.remaining.compare_exchange_weak(
                left,
                left - granted,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return granted,
                Err(actual) => left = actual,
            }
        }
    }

    pub fn remaining(&self) -> usize {
        self.remaining.load(Ordering::Relaxed)
    }
}

impl Default for SearchBudget {
    fn default() -> Self {
        Self::new(QUERIES_PER_REQUEST)
    }
}

/// Duffel searches allowed per user request. Every one is billed — with no
/// bookings the 1500-per-order allowance is zero — and a model comparing
/// dates will happily ask for a dozen. Enough for a return plus a few
/// nearby days, and a ceiling on a loop that has lost its way. Eight
/// because a ±3 day flexible search is seven, and leaving no headroom
/// would make the commonest flexible question spend the whole allowance.
pub const FLIGHT_SEARCHES_PER_REQUEST: usize = 8;

/// A per-request allowance of Duffel searches, plus a memo of what each one
/// returned.
///
/// The memo is what actually saves money: within one request the model
/// re-asks the same route while it works through a comparison, and those
/// repeats are a second charge for an answer already in hand. Seconds
/// apart, the offers are the same offers.
#[derive(Debug, Default)]
pub struct FlightBudget {
    spent: AtomicUsize,
    seen: std::sync::Mutex<std::collections::HashMap<String, Vec<crate::tools::duffel::Flight>>>,
}

impl FlightBudget {
    /// What an earlier search in this request already found, if any.
    pub fn recall(&self, key: &str) -> Option<Vec<crate::tools::duffel::Flight>> {
        self.seen.lock().unwrap().get(key).cloned()
    }

    /// Reserves one search. False when the request has spent its allowance,
    /// in which case nothing should be sent to Duffel.
    pub fn claim_one(&self) -> bool {
        // Compare-and-swap rather than fetch_add: a refused claim must not
        // still count, or `spent` overstates what was actually billed.
        let mut spent = self.spent.load(Ordering::Relaxed);
        loop {
            if spent >= FLIGHT_SEARCHES_PER_REQUEST {
                return false;
            }
            match self.spent.compare_exchange_weak(
                spent,
                spent + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => spent = actual,
            }
        }
    }

    pub fn remember(&self, key: String, flights: Vec<crate::tools::duffel::Flight>) {
        self.seen.lock().unwrap().insert(key, flights);
    }

    pub fn spent(&self) -> usize {
        self.spent.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod flight_budget_tests {
    use super::*;

    #[test]
    fn searches_are_allowed_up_to_the_cap_and_then_refused() {
        let budget = FlightBudget::default();
        for _ in 0..FLIGHT_SEARCHES_PER_REQUEST {
            assert!(budget.claim_one());
        }
        assert!(!budget.claim_one(), "the cap is what stops a runaway loop");
        assert_eq!(budget.spent(), FLIGHT_SEARCHES_PER_REQUEST);
    }

    #[test]
    fn a_remembered_search_comes_back_without_spending_anything() {
        let budget = FlightBudget::default();
        assert_eq!(budget.recall("AMS-LIS"), None);
        budget.remember("AMS-LIS".to_string(), Vec::new());
        assert_eq!(budget.recall("AMS-LIS"), Some(Vec::new()));
        assert_eq!(budget.spent(), 0, "recall must not count as a search");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_are_granted_until_the_budget_runs_out() {
        let budget = SearchBudget::new(5);
        assert_eq!(budget.claim(3), 3);
        assert_eq!(budget.remaining(), 2);
        // more than is left: granted down to what remains
        assert_eq!(budget.claim(3), 2);
        assert_eq!(budget.remaining(), 0);
        assert_eq!(budget.claim(1), 0);
    }

    #[test]
    fn a_zero_claim_never_reserves_anything() {
        let budget = SearchBudget::new(2);
        assert_eq!(budget.claim(0), 0);
        assert_eq!(budget.remaining(), 2);
    }

    #[tokio::test]
    async fn concurrent_claims_never_oversell() {
        // search_secondhand claims from several site futures at once.
        let budget = std::sync::Arc::new(SearchBudget::new(10));
        let granted: usize = futures::future::join_all((0..20).map(|_| {
            let budget = budget.clone();
            async move { tokio::task::spawn_blocking(move || budget.claim(1)).await.unwrap() }
        }))
        .await
        .into_iter()
        .sum();
        assert_eq!(granted, 10);
        assert_eq!(budget.remaining(), 0);
    }
}
