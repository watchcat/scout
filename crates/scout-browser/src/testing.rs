//! What the tests that need a browser share: the Chrome, a limit on how
//! many run at once, and a page server on loopback.
//!
//! A session is never tested against a shop. Every page here is served by
//! the test itself.

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The Chrome to test with, or `None` after saying so where it is seen.
///
/// A machine without a Chrome must not fail these tests, and must not pass
/// them quietly either. The line is written to the process's own stderr,
/// which the test harness does not capture, so it shows in an ordinary
/// `cargo test` beside a test that the harness can only call "ok".
pub fn chrome_or_skip() -> Option<PathBuf> {
    let found = crate::chrome::find_chrome();
    if found.is_none() {
        // The harness names each test's thread after the test.
        let thread = std::thread::current();
        let test = thread.name().unwrap_or("a test");
        let _ = writeln!(std::io::stderr(), "SKIPPED, NOT RUN: {test}: no Chrome on this machine (set SCOUT_CHROME to one)");
    }
    found
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
