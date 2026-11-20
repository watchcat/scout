//! What the tests share: the Chrome, a limit on how many run at once, a
//! page server on loopback, and a browser that is no browser at all.
//!
//! A session is never tested against a shop. Every page here is served by
//! the test itself.

use crate::drive::{Action, Broken, Browser, Done, Page, Seen};
use crate::limits::Limits;
use crate::session::Sessions;
use crate::site::Shop;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use url::Url;

/// The Chrome to test with, or `None` after saying so where it is seen.
///
/// A machine without a Chrome must not fail these tests, and must not pass
/// them quietly either. The line is written to the process's own stderr,
/// which the test harness does not capture, so it shows in an ordinary
/// `cargo test` beside a test that the harness can only call "ok".
///
/// That is still a green run of tests that did not run. Where the browser
/// tests are the point, `SCOUT_REQUIRE_CHROME` set to anything makes a
/// missing Chrome a failure of each test that wanted one.
pub fn chrome_or_skip() -> Option<PathBuf> {
    let required = std::env::var_os("SCOUT_REQUIRE_CHROME").is_some_and(|value| !value.is_empty());
    found_or_skipped(crate::chrome::find_chrome(), required)
}

fn found_or_skipped(found: Option<PathBuf>, required: bool) -> Option<PathBuf> {
    if found.is_none() {
        // The harness names each test's thread after the test.
        let thread = std::thread::current();
        let test = thread.name().unwrap_or("a test");
        assert!(!required, "{test}: SCOUT_REQUIRE_CHROME is set and there is no Chrome on this machine (set SCOUT_CHROME to one)");
        let _ = writeln!(std::io::stderr(), "SKIPPED, NOT RUN: {test}: no Chrome on this machine (set SCOUT_CHROME to one)");
    }
    found
}

#[test]
#[should_panic(expected = "SCOUT_REQUIRE_CHROME is set and there is no Chrome")]
fn a_chrome_that_is_required_and_missing_fails_the_test_that_wanted_it() {
    found_or_skipped(None, true);
}

#[test]
fn a_chrome_that_is_there_is_the_one_tested_with_required_or_not() {
    let chrome = PathBuf::from("/a/chrome");
    assert_eq!(found_or_skipped(Some(chrome.clone()), true), Some(chrome.clone()));
    assert_eq!(found_or_skipped(Some(chrome.clone()), false), Some(chrome));
}

/// A place among the Chromes running at once in this test binary. Each
/// test starts its own, and a dozen at once on a laptop is how a press
/// takes longer than the page was given to answer it.
pub async fn gate() -> tokio::sync::SemaphorePermit<'static> {
    static GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
    GATE.acquire().await.expect("the gate is never closed")
}

/// What a test's server answers one path with.
#[derive(Clone)]
pub struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
}

/// A page.
pub fn html(body: &str) -> Reply {
    Reply { status: 200, headers: vec![("content-type", "text/html; charset=utf-8".into())], body: body.into() }
}

/// A page with another status than 200.
pub fn status(status: u16, body: &str) -> Reply {
    Reply { status, ..html(body) }
}

/// A redirect.
pub fn redirect(to: &str) -> Reply {
    Reply { status: 302, headers: vec![("location", to.into())], body: String::new() }
}

/// A file the server wants saved, not shown.
pub fn attachment(name: &str, body: &str) -> Reply {
    Reply {
        status: 200,
        headers: vec![("content-type", "application/octet-stream".into()), ("content-disposition", format!("attachment; filename=\"{name}\""))],
        body: body.into(),
    }
}

/// A server on a loopback port of its own, and what it was asked for.
pub struct Pages {
    pub addr: SocketAddr,
    asked: Arc<Mutex<Vec<String>>>,
    serving: tokio::task::JoinHandle<()>,
}

impl Pages {
    /// Serves `routes` by path. In a body, `{base}` is written as the
    /// server's own address, so a page can link to itself in full.
    pub async fn serve(routes: &[(&str, Reply)]) -> Pages {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let routes: Arc<HashMap<String, Reply>> = Arc::new(
            routes
                .iter()
                .map(|(path, reply)| {
                    let mut reply = reply.clone();
                    reply.body = reply.body.replace("{base}", &base);
                    for (_, value) in &mut reply.headers {
                        *value = value.replace("{base}", &base);
                    }
                    (path.to_string(), reply)
                })
                .collect(),
        );
        let asked: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = asked.clone();
        let app = axum::Router::new().fallback(move |request: Request<Body>| {
            let (routes, log) = (routes.clone(), log.clone());
            async move {
                log.lock().unwrap().push(format!("{} {}", request.method(), request.uri()));
                let Some(reply) = routes.get(request.uri().path()) else {
                    return Response::builder().status(StatusCode::NOT_FOUND).body(Body::from("<title>Not found</title>")).unwrap();
                };
                let mut response = Response::builder().status(reply.status);
                for (name, value) in &reply.headers {
                    response = response.header(*name, value);
                }
                response.body(Body::from(reply.body.clone())).unwrap()
            }
        });
        let serving = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Pages { addr, asked, serving }
    }

    /// The address of `path` on this server.
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// Every request so far, as `GET /path`, less the icon a browser asks
    /// for unbidden.
    pub fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().iter().filter(|line| !line.ends_with("/favicon.ico")).cloned().collect()
    }

    /// Whether anything asked for `path`.
    pub fn was_asked(&self, path: &str) -> bool {
        self.asked().iter().any(|line| line.split(' ').nth(1).is_some_and(|asked| asked.split('?').next() == Some(path)))
    }
}

impl Drop for Pages {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

// ------------------------------------------- a browser that is no browser

pub const WIDGET: &str = "https://www.shop.example/p/widget";

/// A browser that answers from a script, at the speed it is told to.
#[derive(Clone, Default)]
pub struct Scripted {
    /// What opening comes to; a page called "Widget" when not said.
    pub opening: Arc<Mutex<Option<Result<Done, Broken>>>>,
    /// What the next actions come to, in order; then pages called "Next".
    pub answers: Arc<Mutex<VecDeque<Result<Done, Broken>>>>,
    /// How long every action takes.
    pub takes: Arc<Mutex<Duration>>,
    pub opened: Arc<AtomicUsize>,
    pub closed: Arc<AtomicUsize>,
    pub asked: Arc<Mutex<Vec<Action>>>,
    /// Set while an action is under way, to catch two at once.
    pub busy: Arc<AtomicBool>,
    pub overlapped: Arc<AtomicBool>,
}

pub struct ScriptedPage(Scripted);

pub fn shown(title: &str) -> Done {
    let html = format!("<html><head><title>{title}</title></head><body>{}</body></html>", "A page of the shop. ".repeat(200));
    Done { seen: Seen { title: title.into(), url: WIDGET.into(), text: format!("# The page\n{title}"), status: Some(200), html, ..Default::default() }, ..Default::default() }
}

pub fn refusing() -> Done {
    let mut done = shown("Shop");
    done.seen.status = Some(403);
    done
}

impl Scripted {
    pub fn then(&self, answers: impl IntoIterator<Item = Result<Done, Broken>>) -> &Self {
        self.answers.lock().unwrap().extend(answers);
        self
    }

    pub fn opening_with(&self, opening: Result<Done, Broken>) -> &Self {
        *self.opening.lock().unwrap() = Some(opening);
        self
    }

    pub fn taking(&self, each: Duration) -> &Self {
        *self.takes.lock().unwrap() = each;
        self
    }

    pub fn sessions(&self, limits: Limits) -> Arc<Sessions<Scripted>> {
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
