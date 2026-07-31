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
