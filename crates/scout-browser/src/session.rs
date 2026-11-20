//! Sessions: what bounds one and what ends it, whatever browser is behind it.
//!
//! The rules of a page are kept where the page is (`tab.rs`). What is kept
//! here needs no browser to be tested: three at once, 25 actions, 180
//! seconds from the call that opened the session, over at a refusal, and
//! nothing done for a session that is over.

use crate::address::{address_refused, Refused};
use crate::drive::{Action, Broken, Browser, Done, Page};
use crate::limits::{Budget, Limits, Slot, Slots, Spent};
use crate::site::Shop;
use crate::wall::{wall, Walls};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;
use url::Url;

/// How long a session that has ended is remembered, so that a call that
/// comes late is told why it ended and not that it never was. Several
/// times a session's whole budget; after it the answer is "no such
/// session", which is also true.
const ENDED_KEPT: Duration = Duration::from_secs(600);

/// A page as whoever drives the session is shown it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Outline {
    pub title: String,
    pub url: String,
    pub text: String,
    /// Whether the page goes on: `read` with `more` gives the next part.
    pub more: bool,
}

/// What opening a session, or one action in it, came to.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Step {
    pub page: Outline,
    /// What did not happen, and why.
    pub refused: Vec<String>,
    /// What else there is to know that the page does not show.
    pub notes: Vec<String>,
    pub actions_left: u32,
    pub seconds_left: u64,
    /// Why the session is over, when this was the last of it.
    pub ended: Option<String>,
}

/// Why there is no step to show.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Failure {
    /// A rule stands in the way of opening that address at all.
    #[error("{0}")]
    Refused(String),
    /// Every place was taken for as long as this session had.
    #[error("{0}")]
    Full(String),
    /// There is no session by that name, or no longer.
    #[error("there is no such session")]
    Unknown,
    /// The session is over, and why.
    #[error("the session has ended: {0}")]
    Ended(String),
    /// The browser or the page failed. The session, if there was one, is over.
    #[error("{0}")]
    Broken(String),
}

/// The sessions that are open, and the places for them.
pub struct Sessions<B: Browser> {
    browser: B,
    limits: Limits,
    slots: Slots,
    all: Mutex<HashMap<String, Arc<Session<B::Page>>>>,
}

struct Session<P> {
    shop: Shop,
    /// One action at a time: a second call waits here for the first.
    state: tokio::sync::Mutex<State<P>>,
}

struct State<P> {
    /// The page and the session's place, until it ends.
    live: Option<(P, Slot)>,
    budget: Budget,
    walls: Walls,
    /// Why it ended, and when.
    ended: Option<(String, Instant)>,
}

impl<P: Page> State<P> {
    /// Ends the session: the browser is stopped and its place given back.
    /// The first reason to end is the one remembered.
    async fn end(&mut self, why: String) {
        if self.ended.is_some() {
            return;
        }
        self.ended = Some((why, Instant::now()));
        if let Some((page, slot)) = self.live.take() {
            page.close().await;
            drop(slot);
        }
    }
}

impl<P: Page> Session<P> {
    /// What a page that came back means for the session, and what whoever
    /// drives it is shown of it.
    async fn judge(&self, state: &mut State<P>, done: Done) -> Result<Step, Failure> {
        let Done { refused, mut notes, seen } = done;
        // The page's own rules keep it on the shop and out of its checkout.
        // Should a page come back from somewhere else all the same, what
        // is on it is not handed on, and nothing more is done there.
        let strayed = Url::parse(&seen.url).map_or(true, |url| address_refused(&self.shop, &url).is_some());
        if strayed {
            let why = "the page was found where a session may not be, and the session is over".to_string();
            state.end(why.clone()).await;
            return Err(Failure::Broken(why));
        }
        let refusal = wall(seen.status, &seen.html, &seen.url);
        state.walls.saw(refusal.is_some());
        if let Some(refusal) = refusal {
            notes.push(refusal.note());
        }
        let now = Instant::now();
        // A refusal first: it is the more telling reason when both are true.
        let over = state.walls.turned_away().map(str::to_string).or_else(|| state.budget.spent(now).map(|spent| spent.to_string()));
        if let Some(why) = &over {
            state.end(why.clone()).await;
        }
        Ok(Step {
            page: Outline { title: seen.title, url: seen.url, text: seen.text, more: seen.more },
            refused,
            notes,
            actions_left: state.budget.actions_left(),
            seconds_left: state.budget.time_left(now).as_secs(),
            ended: over,
        })
    }
}

impl<B: Browser> Sessions<B> {
    pub fn new(browser: B, limits: Limits) -> Arc<Sessions<B>> {
        Arc::new(Sessions { browser, slots: Slots::new(limits.sessions), limits, all: Mutex::default() })
    }

    /// Opens a session on `address` and returns its name with its first page.
    pub async fn open(&self, address: &str) -> Result<(String, Step), Failure> {
        // The clock starts here, before the wait for a place.
        let budget = Budget::starting(Instant::now(), &self.limits);
        let deadline = budget.deadline();
        let refused = |why: Refused| Failure::Refused(why.why().to_string());
        let url = Url::parse(address.trim()).map_err(|_| refused(Refused::NotWeb))?;
        let shop = Shop::of(&url).ok_or_else(|| refused(Refused::NotWeb))?;
        if let Some(why) = address_refused(&shop, &url) {
            return Err(refused(why));
        }
        self.forget_the_long_ended();

        let full = || {
            let (open, seconds) = (self.limits.sessions, self.limits.time.as_secs());
            Failure::Full(format!("{open} sessions are open, and no place came free within this one's {seconds} seconds"))
        };
        let slot = self.slots.take(deadline).await.ok_or_else(full)?;
        // A place that comes free at the moment the time is up is no place.
        if Instant::now() >= deadline {
            return Err(full());
        }
        let (page, done) = match tokio::time::timeout_at(deadline, self.browser.open(&shop, &url)).await {
            Ok(Ok(opened)) => opened,
            Ok(Err(Broken::Refused(why))) => return Err(Failure::Refused(why)),
            Ok(Err(broken)) => return Err(Failure::Broken(broken.to_string())),
            Err(_) => return Err(Failure::Broken("the page did not open within the session's time".to_string())),
        };
        let state = State { live: Some((page, slot)), budget, walls: Walls::default(), ended: None };
        let session = Arc::new(Session { shop, state: tokio::sync::Mutex::new(state) });
        let step = {
            let mut state = session.state.lock().await;
            session.judge(&mut state, done).await?
        };
        let id = format!("{:032x}", rand::random::<u128>());
        self.all.lock().unwrap().insert(id.clone(), session.clone());

        // The session ends when its time does, whether or not anyone calls
        // again: a caller that walks away must not keep a Chrome and a place.
        let (ending, time) = (Arc::downgrade(&session), self.limits.time);
        tokio::spawn(async move {
            tokio::time::sleep_until(deadline).await;
            if let Some(session) = ending.upgrade() {
                session.state.lock().await.end(Spent::Time(time).to_string()).await;
            }
        });
        Ok((id, step))
    }

    /// Does one action in the session named `id`.
    pub async fn act(&self, id: &str, action: Action) -> Result<Step, Failure> {
        let session = self.all.lock().unwrap().get(id).cloned().ok_or(Failure::Unknown)?;
        let mut state = session.state.lock().await;
        if let Some((why, _)) = &state.ended {
            return Err(Failure::Ended(why.clone()));
        }
        if let Err(spent) = state.budget.spend(Instant::now()) {
            state.end(spent.to_string()).await;
            return Err(Failure::Ended(spent.to_string()));
        }
        let deadline = state.budget.deadline();
        let Some((page, _)) = state.live.as_mut() else {
            return Err(Failure::Unknown);
        };
        // The prototype looked at the clock between turns, so a slow turn
        // overran. Here an action is cut at the session's last second.
        let done = match tokio::time::timeout_at(deadline, page.act(&action)).await {
            Ok(Ok(done)) => done,
            Ok(Err(broken)) => {
                state.end(broken.to_string()).await;
                return Err(Failure::Broken(broken.to_string()));
            }
            Err(_) => {
                let spent = Spent::Time(self.limits.time).to_string();
                state.end(spent.clone()).await;
                return Err(Failure::Ended(spent));
            }
        };
        session.judge(&mut state, done).await
    }

    /// Closes the session named `id` and forgets it.
    pub async fn close(&self, id: &str) -> Result<(), Failure> {
        let session = self.all.lock().unwrap().remove(id).ok_or(Failure::Unknown)?;
        session.state.lock().await.end("it was closed".to_string()).await;
        Ok(())
    }

    /// Places not taken by a session.
    pub fn free(&self) -> usize {
        self.slots.free()
    }

    fn forget_the_long_ended(&self) {
        let now = Instant::now();
        self.all.lock().unwrap().retain(|_, session| match session.state.try_lock() {
            Ok(state) => !state.ended.as_ref().is_some_and(|(_, at)| now.duration_since(*at) >= ENDED_KEPT),
            // Busy with an action, so not ended.
            Err(_) => true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::{MAX_ACTIONS, MAX_SESSIONS, SESSION_BUDGET};
    use scout_core::net_guard::NOT_PUBLIC;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const WIDGET: &str = "https://www.shop.example/p/widget";

    /// A browser that answers from a script, at the speed it is told to.
    #[derive(Clone, Default)]
    struct Scripted {
        /// What opening comes to; a page called "Widget" when not said.
        opening: Arc<Mutex<Option<Result<Done, Broken>>>>,
        /// What the next actions come to, in order; then pages called "Next".
        answers: Arc<Mutex<VecDeque<Result<Done, Broken>>>>,
        /// How long every action takes.
        takes: Arc<Mutex<Duration>>,
        opened: Arc<AtomicUsize>,
        closed: Arc<AtomicUsize>,
        asked: Arc<Mutex<Vec<Action>>>,
        /// Set while an action is under way, to catch two at once.
        busy: Arc<AtomicBool>,
        overlapped: Arc<AtomicBool>,
    }

    struct ScriptedPage(Scripted);

    fn shown(title: &str) -> Done {
        let html = format!("<html><head><title>{title}</title></head><body>{}</body></html>", "A page of the shop. ".repeat(200));
        Done { seen: crate::drive::Seen { title: title.into(), url: WIDGET.into(), text: format!("# The page\n{title}"), status: Some(200), html, ..Default::default() }, ..Default::default() }
    }

    fn refusing() -> Done {
        let mut done = shown("Shop");
        done.seen.status = Some(403);
        done
    }

    impl Scripted {
        fn then(&self, answers: impl IntoIterator<Item = Result<Done, Broken>>) -> &Self {
            self.answers.lock().unwrap().extend(answers);
            self
        }

        fn opening_with(&self, opening: Result<Done, Broken>) -> &Self {
            *self.opening.lock().unwrap() = Some(opening);
            self
        }

        fn taking(&self, each: Duration) -> &Self {
            *self.takes.lock().unwrap() = each;
            self
        }

        fn sessions(&self, limits: Limits) -> Arc<Sessions<Scripted>> {
            Sessions::new(self.clone(), limits)
        }
    }

    impl Browser for Scripted {
        type Page = ScriptedPage;

        async fn open(&self, _shop: &Shop, _url: &Url) -> Result<(ScriptedPage, Done), Broken> {
            self.opened.fetch_add(1, Ordering::SeqCst);
            let opening = self.opening.lock().unwrap().clone().unwrap_or_else(|| Ok(shown("Widget")));
            opening.map(|done| (ScriptedPage(self.clone()), done))
        }
    }

    impl Page for ScriptedPage {
        async fn act(&mut self, action: &Action) -> Result<Done, Broken> {
            if self.0.busy.swap(true, Ordering::SeqCst) {
                self.0.overlapped.store(true, Ordering::SeqCst);
            }
            self.0.asked.lock().unwrap().push(action.clone());
            let takes = *self.0.takes.lock().unwrap();
            tokio::time::sleep(takes).await;
            self.0.busy.store(false, Ordering::SeqCst);
            let scripted = self.0.answers.lock().unwrap().pop_front();
            scripted.unwrap_or_else(|| Ok(shown("Next")))
        }

        async fn close(self) {
            self.0.closed.fetch_add(1, Ordering::SeqCst);
        }
    }

    const READ: Action = Action::Read { more: false };

    #[tokio::test]
    async fn a_session_is_opened_acted_in_and_closed() {
        let browser = Scripted::default();
        let sessions = browser.sessions(Limits::default());
        let (id, first) = sessions.open(WIDGET).await.unwrap();
        assert_eq!(first.page, Outline { title: "Widget".into(), url: WIDGET.into(), text: "# The page\nWidget".into(), more: false });
        assert_eq!((first.actions_left, first.ended.clone()), (MAX_ACTIONS, None));
        assert!(first.seconds_left <= 180 && first.seconds_left >= 179, "{}", first.seconds_left);
        assert_eq!(sessions.free(), MAX_SESSIONS - 1);

        let step = sessions.act(&id, Action::Click { n: 3 }).await.unwrap();
        assert_eq!((step.page.title.as_str(), step.actions_left, step.ended), ("Next", MAX_ACTIONS - 1, None));
        assert_eq!(*browser.asked.lock().unwrap(), [Action::Click { n: 3 }]);

        assert_eq!(sessions.close(&id).await, Ok(()));
        assert_eq!((browser.closed.load(Ordering::SeqCst), sessions.free()), (1, MAX_SESSIONS));
        // Closed is gone: nothing is remembered of it.
        assert_eq!(sessions.act(&id, READ).await, Err(Failure::Unknown));
        assert_eq!(sessions.close(&id).await, Err(Failure::Unknown));
        assert_eq!(sessions.act("no-such-session", READ).await, Err(Failure::Unknown));
    }

    #[tokio::test]
    async fn every_session_has_a_name_of_its_own_that_cannot_be_counted_up_to() {
        let sessions = Scripted::default().sessions(Limits::default());
        let (a, _) = sessions.open(WIDGET).await.unwrap();
        let (b, _) = sessions.open(WIDGET).await.unwrap();
        assert_ne!(a, b);
        assert!(a.len() >= 32 && a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
    }

    #[tokio::test]
    async fn an_address_that_may_not_be_opened_never_reaches_a_browser() {
        let browser = Scripted::default();
        let sessions = browser.sessions(Limits::default());
        for (address, why) in [
            ("not an address", Refused::NotWeb),
            ("file:///etc/passwd", Refused::NotWeb),
            ("about:blank", Refused::NotWeb),
            ("https://www.shop.example/checkout", Refused::Checkout),
            ("https://checkout.shop.example/", Refused::Checkout),
            ("https://www.shop.example/account/login", Refused::SignIn),
        ] {
            assert_eq!(sessions.open(address).await.err(), Some(Failure::Refused(why.why().to_string())), "{address}");
        }
        assert_eq!((browser.opened.load(Ordering::SeqCst), sessions.free()), (0, MAX_SESSIONS));
    }

    #[tokio::test]
    async fn an_address_the_browser_will_not_or_cannot_open_takes_no_place() {
        let browser = Scripted::default();
        let sessions = browser.sessions(Limits::default());
        browser.opening_with(Err(Broken::Refused(NOT_PUBLIC.to_string())));
        assert_eq!(sessions.open(WIDGET).await.err(), Some(Failure::Refused(NOT_PUBLIC.to_string())));
        browser.opening_with(Err(Broken::Unreachable("net::ERR_NAME_NOT_RESOLVED".to_string())));
        assert_eq!(sessions.open(WIDGET).await.err(), Some(Failure::Broken("the page could not be opened: net::ERR_NAME_NOT_RESOLVED".to_string())));
        browser.opening_with(Err(Broken::Failed("went away".to_string())));
        assert_eq!(sessions.open(WIDGET).await.err(), Some(Failure::Broken("the browser failed: went away".to_string())));
        assert_eq!(sessions.free(), MAX_SESSIONS);
    }

    #[tokio::test]
    async fn the_25th_action_is_the_last_and_a_26th_is_refused() {
        let browser = Scripted::default();
        let sessions = browser.sessions(Limits::default());
        let (id, _) = sessions.open(WIDGET).await.unwrap();
        for taken in 1..MAX_ACTIONS {
            let step = sessions.act(&id, READ).await.unwrap();
            assert_eq!((step.actions_left, step.ended), (MAX_ACTIONS - taken, None), "action {taken}");
        }
        // The last one is done, and says it was the last.
        let last = sessions.act(&id, READ).await.unwrap();
        assert_eq!((last.actions_left, last.ended.as_deref(), last.page.title.as_str()), (0, Some("its 25 actions are spent"), "Next"));
        // With nothing left to do, the browser and the place are given back at once.
        assert_eq!((browser.closed.load(Ordering::SeqCst), sessions.free()), (1, MAX_SESSIONS));

        assert_eq!(sessions.act(&id, READ).await, Err(Failure::Ended("its 25 actions are spent".to_string())));
        assert_eq!(browser.asked.lock().unwrap().len(), MAX_ACTIONS as usize, "the 26th never reached the page");
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_is_over_when_its_180_seconds_are_whoever_is_or_is_not_calling() {
        let browser = Scripted::default();
        let sessions = browser.sessions(Limits::default());
        let (id, _) = sessions.open(WIDGET).await.unwrap();
        tokio::time::sleep(Duration::from_secs(179)).await;
        let step = sessions.act(&id, READ).await.unwrap();
        assert_eq!((step.seconds_left, step.ended), (1, None));
        assert_eq!((browser.closed.load(Ordering::SeqCst), sessions.free()), (0, MAX_SESSIONS - 1));

        // Nobody calls. The session ends itself and gives its place back.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!((browser.closed.load(Ordering::SeqCst), sessions.free()), (1, MAX_SESSIONS));
        assert_eq!(sessions.act(&id, READ).await, Err(Failure::Ended("its 180 seconds are spent".to_string())));
        assert_eq!(browser.asked.lock().unwrap().len(), 1, "an action after the time never reached the page");
    }

    #[tokio::test(start_paused = true)]
    async fn an_action_still_under_way_when_the_time_is_up_is_cut_there() {
        let browser = Scripted::default();
        browser.taking(Duration::from_secs(300));
        let sessions = browser.sessions(Limits::default());
        let started = Instant::now();
        let (id, _) = sessions.open(WIDGET).await.unwrap();
        // The prototype looked at the clock between turns, so a slow one overran.
        assert_eq!(sessions.act(&id, READ).await, Err(Failure::Ended("its 180 seconds are spent".to_string())));
        assert_eq!(started.elapsed(), SESSION_BUDGET);
        assert_eq!((browser.closed.load(Ordering::SeqCst), sessions.free()), (1, MAX_SESSIONS));
    }

    #[tokio::test(start_paused = true)]
    async fn a_fourth_session_waits_out_its_own_budget_and_is_then_refused() {
        let browser = Scripted::default();
        let sessions = browser.sessions(Limits::default());
        let mut held = Vec::new();
        for _ in 0..MAX_SESSIONS {
            held.push(sessions.open(WIDGET).await.unwrap().0);
        }
        let started = Instant::now();
        let fourth = sessions.open(WIDGET).await.err();
        assert_eq!(fourth, Some(Failure::Full("3 sessions are open, and no place came free within this one's 180 seconds".to_string())));
        assert_eq!(started.elapsed(), SESSION_BUDGET);
        assert_eq!(browser.opened.load(Ordering::SeqCst), MAX_SESSIONS, "no fourth browser was started");
    }

    #[tokio::test(start_paused = true)]
    async fn a_place_given_back_goes_to_the_session_that_was_waiting_with_what_is_left_of_its_time() {
        let browser = Scripted::default();
        let sessions = browser.sessions(Limits::default());
        let mut held = Vec::new();
        for _ in 0..MAX_SESSIONS {
            held.push(sessions.open(WIDGET).await.unwrap().0);
        }
        let waiting = tokio::spawn({
            let sessions = sessions.clone();
            async move { sessions.open(WIDGET).await }
        });
        tokio::time::sleep(Duration::from_secs(40)).await;
        assert!(!waiting.is_finished());
        sessions.close(&held[0]).await.unwrap();
        let (_, first) = waiting.await.unwrap().expect("the place went to the one waiting");
        // Its clock started when it asked, not when it was let in.
        assert_eq!(first.seconds_left, 140);
        assert_eq!(sessions.free(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn each_of_two_sessions_at_once_is_held_to_its_own_limits() {
        let browser = Scripted::default();
        let limits = Limits { sessions: 2, actions: 3, time: Duration::from_secs(60) };
        let sessions = browser.sessions(limits);
        let (a, _) = sessions.open(WIDGET).await.unwrap();
        tokio::time::sleep(Duration::from_secs(20)).await;
        let (b, _) = sessions.open(WIDGET).await.unwrap();

        // One spends its actions; the other's are untouched by it.
        for _ in 0..3 {
            sessions.act(&a, READ).await.unwrap();
        }
        assert_eq!(sessions.act(&a, READ).await, Err(Failure::Ended("its 3 actions are spent".to_string())));
        let step = sessions.act(&b, READ).await.unwrap();
        assert_eq!((step.actions_left, step.seconds_left, step.ended), (2, 60, None));
        assert_eq!(sessions.free(), 1, "the one that ended gave its place back, the other holds its own");

        // And each has its own clock: the second outlives the first's deadline.
        tokio::time::sleep(Duration::from_secs(45)).await;
        let step = sessions.act(&b, READ).await.unwrap();
        assert_eq!((step.actions_left, step.seconds_left), (1, 15));
        tokio::time::sleep(Duration::from_secs(16)).await;
        assert_eq!(sessions.act(&b, READ).await, Err(Failure::Ended("its 60 seconds are spent".to_string())));
        assert_eq!((browser.closed.load(Ordering::SeqCst), sessions.free()), (2, 2));
    }

    #[tokio::test]
    async fn two_calls_on_one_session_are_taken_one_after_the_other() {
        let browser = Scripted::default();
        browser.taking(Duration::from_millis(30));
        let sessions = browser.sessions(Limits::default());
        let (id, _) = sessions.open(WIDGET).await.unwrap();
        let (one, two) = tokio::join!(sessions.act(&id, READ), sessions.act(&id, Action::Back));
        assert!(one.is_ok() && two.is_ok());
        assert!(!browser.overlapped.load(Ordering::SeqCst), "the page was asked two things at once");
        assert_eq!(browser.asked.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_session_opened_on_a_page_the_shop_refuses_is_over_before_it_began() {
        let browser = Scripted::default();
        browser.opening_with(Ok(refusing()));
        let sessions = browser.sessions(Limits::default());
        let (id, first) = sessions.open(WIDGET).await.unwrap();
        // The page is shown, with what it is, and that is the end.
        assert_eq!(first.ended.as_deref(), Some("the page the session was opened on was refused"));
        assert_eq!(first.notes, ["This page is a refusal (HTTP 403). Nothing is done to get past it."]);
        assert_eq!((browser.closed.load(Ordering::SeqCst), sessions.free()), (1, MAX_SESSIONS));

        for action in [READ, Action::Click { n: 1 }, Action::Back, Action::Open { url: WIDGET.into() }] {
            assert_eq!(sessions.act(&id, action).await, Err(Failure::Ended("the page the session was opened on was refused".to_string())));
        }
        assert_eq!(browser.asked.lock().unwrap().len(), 0, "nothing after the end reached the page");
    }

    #[tokio::test]
    async fn a_session_ends_at_two_refusals_running_and_not_at_one() {
        let browser = Scripted::default();
        browser.then([Ok(refusing()), Ok(shown("Delivery")), Ok(refusing()), Ok(refusing())]);
        let sessions = browser.sessions(Limits::default());
        let (id, _) = sessions.open(WIDGET).await.unwrap();
        // One side page that would not open is only that.
        let step = sessions.act(&id, READ).await.unwrap();
        assert_eq!((step.ended, step.notes.len()), (None, 1));
        let step = sessions.act(&id, READ).await.unwrap();
        assert_eq!((step.ended, step.notes.len()), (None, 0));
        let step = sessions.act(&id, READ).await.unwrap();
        assert_eq!(step.ended, None);
        let step = sessions.act(&id, READ).await.unwrap();
        assert_eq!(step.ended.as_deref(), Some("the shop refused two pages running"));
        assert_eq!((browser.closed.load(Ordering::SeqCst), sessions.free()), (1, MAX_SESSIONS));
        assert_eq!(sessions.act(&id, READ).await, Err(Failure::Ended("the shop refused two pages running".to_string())));
        assert_eq!(browser.asked.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn a_session_ends_at_the_third_refusal_however_they_fall() {
        let browser = Scripted::default();
        browser.then([Ok(refusing()), Ok(shown("A")), Ok(refusing()), Ok(shown("B")), Ok(refusing())]);
        let sessions = browser.sessions(Limits::default());
        let (id, _) = sessions.open(WIDGET).await.unwrap();
        for _ in 0..4 {
            assert_eq!(sessions.act(&id, READ).await.unwrap().ended, None);
        }
        assert_eq!(sessions.act(&id, READ).await.unwrap().ended.as_deref(), Some("the shop refused three pages"));
    }

    #[tokio::test]
    async fn a_browser_that_fails_is_the_end_of_its_session() {
        let browser = Scripted::default();
        browser.then([Err(Broken::Failed("did not answer".to_string()))]);
        let sessions = browser.sessions(Limits::default());
        let (id, _) = sessions.open(WIDGET).await.unwrap();
        assert_eq!(sessions.act(&id, READ).await, Err(Failure::Broken("the browser failed: did not answer".to_string())));
        assert_eq!((browser.closed.load(Ordering::SeqCst), sessions.free()), (1, MAX_SESSIONS));
        assert_eq!(sessions.act(&id, READ).await, Err(Failure::Ended("the browser failed: did not answer".to_string())));
    }

    #[tokio::test]
    async fn a_page_found_off_the_shop_is_not_shown_and_ends_the_session() {
        // The page's own rules should never let this be. If they do, what
        // is on such a page is not handed on.
        let browser = Scripted::default();
        let mut elsewhere = shown("Another shop");
        elsewhere.seen.url = "https://other.example/landed".into();
        let mut checkout = shown("Checkout");
        checkout.seen.url = "https://www.shop.example/checkout".into();
        for strayed in [elsewhere, checkout] {
            browser.then([Ok(strayed)]);
            let sessions = browser.sessions(Limits::default());
            let (id, _) = sessions.open(WIDGET).await.unwrap();
            let failed = sessions.act(&id, READ).await.unwrap_err();
            assert_eq!(failed, Failure::Broken("the page was found where a session may not be, and the session is over".to_string()));
            assert_eq!(sessions.free(), MAX_SESSIONS);
            assert!(matches!(sessions.act(&id, READ).await, Err(Failure::Ended(_))));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_that_ended_is_remembered_for_a_while_and_then_forgotten() {
        let browser = Scripted::default();
        browser.opening_with(Ok(refusing()));
        let sessions = browser.sessions(Limits::default());
        let (id, _) = sessions.open(WIDGET).await.unwrap();
        tokio::time::sleep(ENDED_KEPT - Duration::from_secs(1)).await;
        sessions.open(WIDGET).await.unwrap();
        assert!(matches!(sessions.act(&id, READ).await, Err(Failure::Ended(_))));
        tokio::time::sleep(Duration::from_secs(2)).await;
        // The next session opened sweeps out the ones long over.
        sessions.open(WIDGET).await.unwrap();
        assert_eq!(sessions.act(&id, READ).await, Err(Failure::Unknown));
    }
}
