//! What bounds a session: how many at once, how many actions, how long.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

/// Sessions open at once. The spec's number, chosen with the pod's memory
/// limit (2 Gi with sessions) and not measured: each session is a Chrome
/// of its own, as each render is.
pub const MAX_SESSIONS: usize = 3;

/// Actions one session may take, a read counted like a press. The spec's
/// number. The sixteen sessions of the reading that a shop did not refuse
/// took 10 to 15 (`docs/2026-11-18-shop-reach.md`), so this is room, not a
/// squeeze.
pub const MAX_ACTIONS: u32 = 25;

/// How long one session lasts from the call that opens it, the wait for a
/// free slot included. The spec said 120 seconds; the reading says nearer
/// 180: of those sixteen sessions 4 ran past 120 s (median 78 s, 39 to
/// 225), and of seventeen with the one that died on the model's own
/// timeout, two outran 180 as well. Most of a session's time is the model
/// thinking between actions, and it counts here: the clock is the wall's.
pub const SESSION_BUDGET: Duration = Duration::from_secs(180);

/// The three numbers, so that a test can run a session to its end in less
/// than three minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub sessions: usize,
    pub actions: u32,
    pub time: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { sessions: MAX_SESSIONS, actions: MAX_ACTIONS, time: SESSION_BUDGET }
    }
}

/// Which of a session's two allowances ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spent {
    Actions(u32),
    Time(Duration),
}

impl std::fmt::Display for Spent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Spent::Actions(n) => write!(f, "its {n} actions are spent"),
            Spent::Time(d) => write!(f, "its {} seconds are spent", d.as_secs()),
        }
    }
}

/// What one session has left of its actions and its time.
#[derive(Debug)]
pub struct Budget {
    limits: Limits,
    deadline: Instant,
    actions_left: u32,
}

impl Budget {
    /// A whole budget, its clock started at `at`: when the call to open the
    /// session arrived, not when a slot came free for it.
    pub fn starting(at: Instant, limits: &Limits) -> Budget {
        Budget { limits: *limits, deadline: at + limits.time, actions_left: limits.actions }
    }

    /// Takes one action off the budget, or says what has run out.
    pub fn spend(&mut self, now: Instant) -> Result<(), Spent> {
        if now >= self.deadline {
            return Err(Spent::Time(self.limits.time));
        }
        if self.actions_left == 0 {
            return Err(Spent::Actions(self.limits.actions));
        }
        self.actions_left -= 1;
        Ok(())
    }

    /// What has run out by `now`, if anything: after the last action a
    /// session has nothing left to do, and after its time nothing either.
    pub fn spent(&self, now: Instant) -> Option<Spent> {
        if now >= self.deadline {
            Some(Spent::Time(self.limits.time))
        } else if self.actions_left == 0 {
            Some(Spent::Actions(self.limits.actions))
        } else {
            None
        }
    }

    pub fn actions_left(&self) -> u32 {
        self.actions_left
    }

    pub fn time_left(&self, now: Instant) -> Duration {
        self.deadline.saturating_duration_since(now)
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }
}

/// The places for sessions open at once.
#[derive(Debug, Clone)]
pub struct Slots {
    free: Arc<Semaphore>,
}

/// One session's place, given back when dropped.
#[derive(Debug)]
pub struct Slot(#[allow(dead_code)] OwnedSemaphorePermit);

impl Slots {
    pub fn new(sessions: usize) -> Slots {
        Slots { free: Arc::new(Semaphore::new(sessions)) }
    }

    /// A place, waited for until `deadline` and no longer. The wait is
    /// inside the session's own budget, as a render's is inside its 45
    /// seconds: a full house cannot turn three minutes into six.
    pub async fn take(&self, deadline: Instant) -> Option<Slot> {
        // `timeout_at` polls the wait once even when the deadline has gone
        // by, and a session with no time left has no use for a place.
        if Instant::now() >= deadline {
            return None;
        }
        match tokio::time::timeout_at(deadline, self.free.clone().acquire_owned()).await {
            Ok(Ok(permit)) => Some(Slot(permit)),
            // The semaphore is never closed, so this is the deadline.
            _ => None,
        }
    }

    pub fn free(&self) -> usize {
        self.free.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL: Limits = Limits { sessions: 3, actions: 4, time: Duration::from_secs(10) };

    #[test]
    fn the_limits_are_the_cards_three_at_once_and_25_actions_and_the_readings_180_seconds() {
        assert_eq!(Limits::default(), Limits { sessions: 3, actions: 25, time: Duration::from_secs(180) });
    }

    #[test]
    fn an_action_past_the_action_limit_is_refused() {
        let start = Instant::now();
        let mut budget = Budget::starting(start, &Limits::default());
        for taken in 1..=MAX_ACTIONS {
            assert_eq!(budget.spend(start), Ok(()), "action {taken}");
            assert_eq!(budget.actions_left(), MAX_ACTIONS - taken);
        }
        assert_eq!(budget.spend(start), Err(Spent::Actions(25)));
        // And goes on being refused: nothing is counted below nothing.
        assert_eq!(budget.spend(start), Err(Spent::Actions(25)));
        assert_eq!(budget.actions_left(), 0);
    }

    #[test]
    fn an_action_past_the_time_limit_is_refused() {
        let start = Instant::now();
        let mut budget = Budget::starting(start, &Limits::default());
        assert_eq!(budget.spend(start + Duration::from_secs(179)), Ok(()));
        assert_eq!(budget.time_left(start + Duration::from_secs(179)), Duration::from_secs(1));
        assert_eq!(budget.spend(start + SESSION_BUDGET), Err(Spent::Time(SESSION_BUDGET)));
        assert_eq!(budget.spend(start + Duration::from_secs(600)), Err(Spent::Time(SESSION_BUDGET)));
        assert_eq!(budget.time_left(start + Duration::from_secs(600)), Duration::ZERO);
        // An action refused for the time is not taken off the count.
        assert_eq!(budget.actions_left(), MAX_ACTIONS - 1);
    }

    #[test]
    fn a_budget_says_when_nothing_is_left_before_anything_more_is_asked() {
        let start = Instant::now();
        let mut budget = Budget::starting(start, &SMALL);
        assert_eq!(budget.spent(start), None);
        for _ in 0..3 {
            budget.spend(start).unwrap();
        }
        assert_eq!(budget.spent(start), None, "one action is left");
        budget.spend(start).unwrap();
        assert_eq!(budget.spent(start), Some(Spent::Actions(4)));
        let fresh = Budget::starting(start, &SMALL);
        assert_eq!(fresh.spent(start + Duration::from_secs(9)), None);
        assert_eq!(fresh.spent(start + Duration::from_secs(10)), Some(Spent::Time(Duration::from_secs(10))));
    }

    #[test]
    fn what_ran_out_is_said_with_its_number() {
        assert_eq!(Spent::Actions(25).to_string(), "its 25 actions are spent");
        assert_eq!(Spent::Time(SESSION_BUDGET).to_string(), "its 180 seconds are spent");
    }

    #[tokio::test(start_paused = true)]
    async fn a_fourth_session_waits_inside_its_own_budget_and_then_gives_up() {
        let slots = Slots::new(MAX_SESSIONS);
        let start = Instant::now();
        let deadline = start + SESSION_BUDGET;
        let held = [slots.take(deadline).await, slots.take(deadline).await, slots.take(deadline).await];
        assert!(held.iter().all(Option::is_some));
        assert_eq!(slots.free(), 0);

        assert!(slots.take(deadline).await.is_none(), "there is no fourth place");
        // It waited, and for its budget and not a moment longer.
        assert_eq!(start.elapsed(), SESSION_BUDGET);
        // Giving up took nothing with it.
        drop(held);
        assert_eq!(slots.free(), MAX_SESSIONS);
    }

    #[tokio::test(start_paused = true)]
    async fn a_place_that_frees_while_a_session_waits_is_taken_at_once() {
        let slots = Slots::new(1);
        let start = Instant::now();
        let first = slots.take(start + SESSION_BUDGET).await.expect("a free place");
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(40)).await;
            drop(first);
        });
        let second = slots.take(start + SESSION_BUDGET).await;
        assert!(second.is_some());
        assert_eq!(start.elapsed(), Duration::from_secs(40));
        assert_eq!(slots.free(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_whose_time_is_already_gone_takes_no_place() {
        let slots = Slots::new(1);
        let start = Instant::now();
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(slots.take(start).await.is_none());
        assert_eq!(slots.free(), 1);
    }
}
