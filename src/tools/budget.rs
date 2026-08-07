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

/// Day-searches a request starts with, before any flexible window.
///
/// Every one is billed, and a model comparing routes will happily ask for a
/// dozen. Four covers a question and a couple of follow-ups; a window that
/// legitimately needs more says so through [`FlightBudget::grant_window`].
///
/// Note this counts *days*, not API calls: a return trip is one request at
/// both providers, so the number of legs costs nothing here. Dates are the
/// only thing that multiplies searches, because neither provider prices a
/// calendar.
pub const BASE_FLIGHT_SEARCHES: usize = 4;

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
    /// Extra days granted for the widest flexible window asked for.
    window: AtomicUsize,
    seen: std::sync::Mutex<std::collections::HashMap<String, Vec<crate::tools::duffel::Flight>>>,
}

impl FlightBudget {
    /// What an earlier search in this request already found, if any.
    pub fn recall(&self, key: &str) -> Option<Vec<crate::tools::duffel::Flight>> {
        self.seen.lock().unwrap().get(key).cloned()
    }

    /// Room for the days either side of a date the traveller asked to see.
    ///
    /// The widest window wins rather than the sum: otherwise asking twice
    /// would buy headroom twice, and a loop could widen its own budget by
    /// repeating itself.
    pub fn grant_window(&self, flex_days: u8) {
        let extra = 2 * usize::from(flex_days);
        self.window.fetch_max(extra, Ordering::Relaxed);
    }

    /// Day-searches this request may buy, given what has been asked for.
    pub fn allowance(&self) -> usize {
        BASE_FLIGHT_SEARCHES + self.window.load(Ordering::Relaxed)
    }

    /// Reserves one search. False when the request has spent its allowance,
    /// in which case nothing should be sent to Duffel.
    pub fn claim_one(&self) -> bool {
        // Compare-and-swap rather than fetch_add: a refused claim must not
        // still count, or `spent` overstates what was actually billed.
        let allowance = self.allowance();
        let mut spent = self.spent.load(Ordering::Relaxed);
        loop {
            if spent >= allowance {
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
    fn a_flexible_window_raises_the_allowance_by_what_it_actually_needs() {
        // A fixed cap has to be either mean to a wide search or generous to
        // a runaway loop. The window is the only thing that legitimately
        // multiplies searches — neither provider prices a calendar, so ±3
        // days is seven separate requests — so the allowance follows it.
        let budget = FlightBudget::default();
        assert_eq!(budget.allowance(), BASE_FLIGHT_SEARCHES);

        budget.grant_window(3);
        assert_eq!(
            budget.allowance(),
            BASE_FLIGHT_SEARCHES + 6,
            "±3 needs six days beyond the one that was asked for"
        );
        // Seven days plus room to follow up, rather than seven and nothing.
        assert!(budget.allowance() > 7);
    }

    #[test]
    fn the_widest_window_sets_the_allowance_rather_than_the_sum_of_them() {
        // Otherwise asking for ±3 twice would buy headroom for thirteen
        // days, and a loop could widen its own budget by repeating itself.
        let budget = FlightBudget::default();
        budget.grant_window(3);
        budget.grant_window(3);
        budget.grant_window(1);
        assert_eq!(budget.allowance(), BASE_FLIGHT_SEARCHES + 6);
    }

    #[test]
    fn a_search_with_no_window_gets_no_extra_room() {
        let budget = FlightBudget::default();
        budget.grant_window(0);
        assert_eq!(budget.allowance(), BASE_FLIGHT_SEARCHES);
        for _ in 0..BASE_FLIGHT_SEARCHES {
            assert!(budget.claim_one());
        }
        assert!(!budget.claim_one());
    }

    #[test]
    fn searches_are_allowed_up_to_the_cap_and_then_refused() {
        let budget = FlightBudget::default();
        for _ in 0..BASE_FLIGHT_SEARCHES {
            assert!(budget.claim_one());
        }
        assert!(!budget.claim_one(), "the cap is what stops a runaway loop");
        assert_eq!(budget.spent(), BASE_FLIGHT_SEARCHES);
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
