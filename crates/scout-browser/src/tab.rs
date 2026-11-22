//! One page of one shop in a Chrome of its own, driven over the pipe.
//!
//! The rules a session keeps are decided in `address.rs` and `typing.rs`;
//! this is where they are held to a real browser. The prototype
//! (`scripts/shop-reach.mjs`) kept some of them loosely and its header says
//! which. What is done about each here:
//!
//! - it refused a link that left the shop but followed a redirect or a
//!   script and then undid it. Here every request for the main frame's
//!   document is held before it is sent, each hop of a redirect included,
//!   and one to a refused address is never sent. A page fetched or made
//!   ready ahead of time is not such a request and would be held by
//!   nothing, so Chrome is told to fetch nothing ahead (`chrome.rs`);
//! - it did not see a new tab. Here another window is closed before it has
//!   fetched anything;
//! - a press landed on whatever lay over the thing pressed. Here the page
//!   is asked what is under the pointer before it comes and again once it
//!   is there, and nothing is pressed unless it is the thing numbered. The
//!   pointer going down and coming up are two things a page hears with no
//!   asking in between, so that much is only known afterwards: a press
//!   found to have landed elsewhere ends the session.

use crate::address::{address_refused, asked_address, link_refused};
use crate::chrome::{chrome_args, Profile};
use crate::drive::{Action, Broken, Browser, Done, Page, Seen, ADDRESS_CHARS, MARKUP_CHARS, OUTLINE_CHARS, OUTLINE_WHOLE, TITLE_CHARS};
use crate::pipe::{CallError, Event, Pipe};
use crate::site::Shop;
use crate::typing::{typing_refused, Field};
use scout_core::net_guard::{serve_socks, Guard, SocksProxy, NOT_PUBLIC};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use url::Url;

/// What runs inside the page.
const IN_PAGE: &str = include_str!("in_page.js");

/// How many of the things a page did unasked are told one by one after an
/// action: messages it put up, most of all. A page can put up a hundred in
/// a second; five say what kind they were, and the rest are a count.
const NOTES_KEPT: usize = 5;

/// How many documents' statuses are remembered, for the way back to them.
const STATUSES_KEPT: usize = 16;

/// How long a page is given to come to rest after something is done to it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pace {
    /// A press that leads somewhere has begun to by then.
    pub begun: Duration,
    /// The longest a page is waited for while it loads.
    pub load: Duration,
    /// What a page's scripts add after it has loaded, they add within this.
    pub quiet: Duration,
}

impl Pace {
    /// The prototype's three waits, which the reading's sessions on ten
    /// shops ran with: 400 ms, then until the page had loaded for at most
    /// twelve seconds, then 900 ms. The numbers are kept as they were, not
    /// tuned. What "loaded" goes by is not the same: the prototype asked
    /// the document, which says "complete" of the old page until the new
    /// one arrives; here Chrome says when the frame has stopped loading.
    pub const MEASURED: Pace =
        Pace { begun: Duration::from_millis(400), load: Duration::from_secs(12), quiet: Duration::from_millis(900) };
}

/// The Chrome sessions run in.
#[derive(Debug, Clone)]
pub struct Chrome {
    path: PathBuf,
    pace: Pace,
    /// A test's own page server: the one loopback socket a session's
    /// Chrome may reach. Outside a test there is no way to set it.
    reaching: Option<SocketAddr>,
}

impl Chrome {
    pub fn new(path: PathBuf) -> Chrome {
        Chrome { path, pace: Pace::MEASURED, reaching: None }
    }

    /// A Chrome for a test: let through to the test's own page server, and
    /// not kept waiting for pages that are a few lines served from memory.
    #[cfg(test)]
    pub(crate) fn reaching(path: PathBuf, server: SocketAddr) -> Chrome {
        let pace = Pace { begun: Duration::from_millis(200), load: Duration::from_secs(10), quiet: Duration::from_millis(100) };
        Chrome { path, pace, reaching: Some(server) }
    }
}

/// Whether `url` may be opened as far as the guard goes: asked before
/// anything is started, so that the answer is the guard's sentence and
/// not a browser's page about a connection that failed. A test's own
/// server is the one address the guard would refuse that passes here.
async fn guard_allows(reaching: Option<SocketAddr>, url: &Url) -> bool {
    let is_the_tests_own = reaching.is_some_and(|server| {
        url.host_str() == Some(server.ip().to_string().as_str()) && url.port_or_known_default() == Some(server.port())
    });
    is_the_tests_own || Guard::PUBLIC_ONLY.check(url).await.is_ok()
}

impl Browser for Chrome {
    type Page = Tab;

    async fn open(&self, shop: &Shop, url: &Url) -> Result<(Tab, Done), Broken> {
        // The proxy would refuse it anyway, but then a Chrome would have
        // been started for a page that cannot be had.
        if !guard_allows(self.reaching, url).await {
            return Err(Broken::Refused(NOT_PUBLIC.to_string()));
        }
        let failed = |e: std::io::Error| Broken::Failed(e.to_string());
        let (port, proxy) = serve_socks(Guard::PUBLIC_ONLY).await.map_err(failed)?;
        let profile = Profile::new().map_err(failed)?;
        let (pipe, said) = Pipe::start(&self.path, &chrome_args(profile.path(), port, self.reaching)).map_err(failed)?;
        let pipe = Arc::new(pipe);
        let (session, frame, target) = match find_page(&pipe).await {
            Ok(found) => found,
            Err(broken) => {
                // Chrome first, then its profile.
                pipe.stop().await;
                profile.remove().await;
                return Err(broken);
            }
        };
        let (loading, _) = watch::channel(false);
        let shared = Arc::new(Shared { happened: Mutex::default(), loading });
        // Listening before anything is switched on, so that nothing Chrome
        // holds for an answer waits for a listener.
        let watched = Watched { shop: shop.clone(), session: session.clone(), frame: frame.clone(), target };
        let watcher = tokio::spawn(watch(pipe.clone(), said, shared.clone(), watched));
        let mut tab = Tab {
            pipe,
            session,
            frame,
            shop: shop.clone(),
            pace: self.pace,
            reaching: self.reaching,
            shared,
            read: None,
            from: 0,
            good: None,
            watcher,
            _proxy: proxy,
            profile,
        };
        let opened = match tab.switch_on().await {
            Ok(()) => tab.first(url).await,
            Err(e) => Err(gone(e)),
        };
        match opened {
            Ok(done) => Ok((tab, done)),
            Err(broken) => {
                tab.close().await;
                Err(broken)
            }
        }
    }
}

/// The tab Chrome starts with: the protocol's session with it, its main
/// frame, and its name among the browser's targets.
async fn find_page(pipe: &Pipe) -> Result<(String, String, String), Broken> {
    // Chrome answers the pipe before it has made its first tab.
    let by = Instant::now() + Duration::from_secs(10);
    let target = loop {
        let targets = pipe.call("Target.getTargets", json!({}), None).await.map_err(gone)?;
        let page = targets["targetInfos"].as_array().and_then(|all| all.iter().find(|t| t["type"] == "page"));
        if let Some(id) = page.and_then(|t| t["targetId"].as_str()) {
            break id.to_string();
        }
        if Instant::now() >= by {
            return Err(Broken::Failed("Chrome opened no page".into()));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let attached = pipe.call("Target.attachToTarget", json!({ "targetId": target, "flatten": true }), None).await.map_err(gone)?;
    let session = attached["sessionId"].as_str().ok_or_else(|| Broken::Failed("Chrome gave no session".into()))?.to_string();
    let tree = pipe.call("Page.getFrameTree", json!({}), Some(&session)).await.map_err(gone)?;
    let frame = tree["frameTree"]["frame"]["id"].as_str().ok_or_else(|| Broken::Failed("Chrome named no frame".into()))?.to_string();
    Ok((session, frame, target))
}

/// What Chrome has said of the page since it was last asked about.
#[derive(Debug, Default)]
struct Happened {
    /// The status each of the main frame's last documents came with, by
    /// the name Chrome gives a document's loading. Kept by document and not
    /// as "the last one heard": going back shows a page again with no
    /// request and no answer, and the last status heard is then another
    /// page's. A handful is enough for the way back.
    statuses: Vec<(String, u16)>,
    refused: Vec<String>,
    notes: Vec<String>,
    /// How many more there were than are kept to be told one by one.
    untold: usize,
    /// Downloads Chrome has reported stopping.
    downloads_denied: u32,
    /// Why the session cannot go on, once it cannot.
    broke: Option<&'static str>,
}

struct Shared {
    happened: Mutex<Happened>,
    /// Whether the main frame is loading.
    loading: watch::Sender<bool>,
}

impl Shared {
    fn refuse(&self, why: String) {
        let mut happened = self.happened.lock().unwrap();
        // A redirect that tries the same door twice is refused once.
        if happened.refused.contains(&why) {
            return;
        }
        if happened.refused.len() < NOTES_KEPT {
            happened.refused.push(why);
        } else {
            happened.untold += 1;
        }
    }

    /// Something the page did that whoever drives is told of: the first
    /// few as they were, the rest as a number.
    fn note(&self, what: String) {
        let mut happened = self.happened.lock().unwrap();
        if happened.notes.len() < NOTES_KEPT {
            happened.notes.push(what);
        } else {
            happened.untold += 1;
        }
    }
}

/// A session's page.
pub struct Tab {
    pipe: Arc<Pipe>,
    /// The protocol's own session with the page, not a browser session.
    session: String,
    /// The main frame, which is the one held to the shop.
    frame: String,
    shop: Shop,
    pace: Pace,
    /// The Chrome's own `reaching`: a test's page server, else nothing.
    reaching: Option<SocketAddr>,
    shared: Arc<Shared>,
    /// The page as it was last read whole.
    read: Option<Read>,
    /// Where in that reading's text the part being shown starts.
    from: usize,
    /// The last address the page was rightly at.
    good: Option<Url>,
    watcher: tokio::task::JoinHandle<()>,
    _proxy: SocksProxy,
    profile: Profile,
}

fn gone(e: CallError) -> Broken {
    Broken::Failed(e.to_string())
}

/// A page as it was read whole. Kept so that reading on hands out more of
/// the same reading: the numbers in it are the ones the page still has.
struct Read {
    title: String,
    url: String,
    /// Where the page was, in full: `url` is cut to a length.
    at: String,
    html: String,
    /// All of the outline's text, up to [`OUTLINE_WHOLE`].
    text: String,
    /// Whether the page's text went on beyond that.
    cut: bool,
    /// Chrome's name for the loading of this document.
    loader: String,
}

/// The thing a number stands for, as `targetInPage` describes it.
#[derive(Debug, serde::Deserialize)]
struct Target {
    /// Where a link goes, in full.
    href: Option<String>,
    /// Whether it is a field for choosing a file.
    upload: bool,
    /// Where a press would land on the thing itself, if anywhere.
    point: Option<Point>,
    /// What lies over it, where something does.
    covered: Option<String>,
    /// Its kind and its words, as the outline writes them after a number.
    said: String,
    /// What the page says of it as a field.
    field: Field,
    /// The same as the page wrote it down, to hold against what the field
    /// says of itself once it has been given the caret.
    key: String,
}

#[derive(Debug, Clone, Copy, serde::Deserialize)]
struct Point {
    x: f64,
    y: f64,
}

impl Tab {
    /// Switches on what a session's rules rest on.
    async fn switch_on(&self) -> Result<(), CallError> {
        self.call("Page.enable", json!({})).await?;
        self.call("Network.enable", json!({})).await?;
        // The page is treated as the window in front, as it is for a
        // person, so that giving a field the caret runs what the page hangs
        // on that there and then. Measured 2026-11-20 on Chrome 157:
        // without this, a field whose own handler passes the caret on to
        // the password field beside it still had the caret when it was
        // looked for, and the test of that page failed.
        self.call("Emulation.setFocusEmulationEnabled", json!({ "enabled": true })).await?;
        // Every document asked for is held until `watch` lets it go.
        self.call("Fetch.enable", json!({ "patterns": [{ "urlPattern": "*", "resourceType": "Document" }] })).await?;
        // Nothing is downloaded, and a file chooser is an event here and
        // not a dialog: nobody answers it, so nothing is uploaded.
        self.pipe.call("Browser.setDownloadBehavior", json!({ "behavior": "deny", "eventsEnabled": true }), None).await?;
        self.call("Page.setInterceptFileChooserDialog", json!({ "enabled": true })).await?;
        // Another window is attached to at birth and held there, before it
        // has run or fetched anything; `watch` closes it.
        let pages = json!([{ "type": "page", "exclude": false }]);
        self.pipe
            .call("Target.setAutoAttach", json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true, "filter": pages }), None)
            .await?;
        Ok(())
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, CallError> {
        self.pipe.call(method, params, Some(&self.session)).await
    }

    /// Runs one of the functions of `in_page.js` in the page and returns
    /// what it returned.
    async fn in_page(&self, function: &str, args: &[Value]) -> Result<Value, CallError> {
        // Asked for each time: the world goes when the page does, and
        // asking again for one that is there returns the same one.
        let world = self.call("Page.createIsolatedWorld", json!({ "frameId": self.frame, "worldName": "scout" })).await?;
        let args = args.iter().map(Value::to_string).collect::<Vec<_>>().join(",");
        let expression = format!("(() => {{ {IN_PAGE}\nreturn {function}({args}) }})()");
        let answer = self
            .call("Runtime.evaluate", json!({ "expression": expression, "contextId": world["executionContextId"], "returnByValue": true }))
            .await?;
        if let Some(thrown) = answer.get("exceptionDetails") {
            let said = thrown["exception"]["description"].as_str().or(thrown["text"].as_str()).unwrap_or("the page's script failed");
            return Err(CallError::Answered(said.to_string()));
        }
        Ok(answer["result"]["value"].clone())
    }

    /// The same, tried again while the page is swapping itself out: a
    /// world asked for in the middle of a navigation is gone by the time
    /// it is used. Three tries 700 ms apart, as the prototype's outline had.
    async fn in_page_patiently(&self, function: &str, args: &[Value]) -> Result<Value, Broken> {
        let mut tries = 0;
        loop {
            match self.in_page(function, args).await {
                Ok(value) => return Ok(value),
                Err(CallError::Answered(_)) if tries < 2 => {
                    tries += 1;
                    tokio::time::sleep(Duration::from_millis(700)).await;
                }
                Err(e) => return Err(gone(e)),
            }
        }
    }

    /// Waits for the page to come to rest after something was done to it.
    async fn settle(&self) {
        tokio::time::sleep(self.pace.begun).await;
        let mut loading = self.shared.loading.subscribe();
        let loaded_by = Instant::now() + self.pace.load;
        loop {
            let _ = tokio::time::timeout_at(loaded_by, loading.wait_for(|loading| !loading)).await;
            tokio::time::sleep(self.pace.quiet).await;
            // A page that sends itself on once it has loaded is loading again.
            if !*loading.borrow() || Instant::now() >= loaded_by {
                return;
            }
        }
    }

    /// Sends the main frame to `url`. `Err` carries Chrome's own word for
    /// why it did not get there. The page has still to come to rest.
    async fn go(&self, url: &Url) -> Result<Result<(), String>, Broken> {
        let went = self.call("Page.navigate", json!({ "url": url.as_str() })).await.map_err(gone)?;
        Ok(match went["errorText"].as_str() {
            Some(error) => Err(error.to_string()),
            None => Ok(()),
        })
    }

    /// Opens the page a session starts on.
    async fn first(&mut self, url: &Url) -> Result<Done, Broken> {
        let went = self.go(url).await?;
        self.settle().await;
        if let Err(error) = went {
            // Refused on the way, by a redirect off the shop or into its
            // checkout: the reason is the rule's and not Chrome's "aborted".
            return Err(match self.shared.happened.lock().unwrap().refused.first() {
                Some(why) => Broken::Refused(why.clone()),
                None => Broken::Unreachable(error),
            });
        }
        let mut refused = Vec::new();
        self.hold(&mut refused).await?;
        self.done(refused, Vec::new()).await
    }

    /// Where the main frame is, by the document that is in it. Chrome's
    /// record of where it was sent would name the address asked for even
    /// when what is shown is Chrome's own page about failing to get there.
    async fn address(&self) -> Result<String, Broken> {
        Ok(self.in_page_patiently("addressInPage", &[]).await?.as_str().unwrap_or_default().to_string())
    }

    /// Checks that the page is where a session may be, and takes it back
    /// to the last such place if it is not.
    ///
    /// The requests Chrome makes are judged before they are sent, but a
    /// page can change its address without one: a route kept after the
    /// hash, or one written by script. Those are found here, after the
    /// fact, and undone.
    async fn hold(&mut self, refused: &mut Vec<String>) -> Result<(), Broken> {
        let at = self.address().await?;
        let why = match Url::parse(&at) {
            // Chrome's page for a page it could not get: the shop did not
            // answer, or the guard would not let the browser dial it.
            Ok(url) if url.scheme() == "chrome-error" => "that page could not be loaded".to_string(),
            Ok(url) => match address_refused(&self.shop, &url) {
                None => {
                    self.good = Some(url);
                    return Ok(());
                }
                Some(why) => why.why().to_string(),
            },
            Err(_) => "that led to no address at all".to_string(),
        };
        let Some(good) = self.good.clone() else {
            return Err(Broken::Refused(why));
        };
        refused.push(format!("{why}; the session went back to the page it was on"));
        let went = self.go(&good).await?;
        self.settle().await;
        if went.is_err() || self.address().await? != good.as_str() {
            return Err(Broken::Failed("the page the session was on could not be opened again".into()));
        }
        Ok(())
    }

    /// What the thing numbered `n` is, or `Err` with why there is none.
    async fn target(&self, n: u32) -> Result<Result<Target, String>, Broken> {
        let found = self.in_page_patiently("targetInPage", &[json!(n)]).await?;
        if let Some(error) = found["error"].as_str() {
            return Ok(Err(error.to_string()));
        }
        serde_json::from_value(found).map(Ok).map_err(|e| Broken::Failed(format!("the page's answer could not be read: {e}")))
    }

    /// Presses the thing numbered `n`, or says in `refused` why it was not.
    async fn click(&mut self, n: u32, refused: &mut Vec<String>, notes: &mut Vec<String>) -> Result<(), Broken> {
        let target = match self.target(n).await? {
            Ok(target) => target,
            Err(why) => {
                refused.push(why);
                return Ok(());
            }
        };
        if target.upload {
            refused.push("that is for choosing a file, and a session uploads nothing".to_string());
            return Ok(());
        }
        // A link says where it goes, so one that may not be followed is
        // not pressed at all. Where a press leads that does not say so
        // beforehand is judged when Chrome asks for it.
        if let Some(why) = target.href.as_deref().and_then(|href| link_refused(&self.shop, href)) {
            refused.push(why.why().to_string());
            return Ok(());
        }
        let Some(at) = target.point else {
            let over = target.covered.unwrap_or_default();
            refused.push(format!("something lies over that and would take the press: \"{over}\""));
            return Ok(());
        };
        // The press is three things to the page: the pointer arriving, going
        // down, and coming up. The page hears each and may answer it.
        self.in_page_patiently("armInPage", &[json!(n)]).await?;
        self.call("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": at.x, "y": at.y })).await.map_err(gone)?;
        // After the first, the page is asked again what is under the
        // pointer: a page that answers its arriving by sliding something
        // else there has not yet been pressed, and is not.
        let hit = self.in_page_patiently("hitInPage", &[json!(n), json!(at.x), json!(at.y)]).await?;
        if hit["on"] != true {
            let _ = self.in_page("pressedInPage", &[]).await;
            let over = hit["over"].as_str().unwrap_or_default();
            refused.push(format!("something came to lie over that as the pointer reached it, and would take the press: \"{over}\""));
            return Ok(());
        }
        let mouse = |kind: &str| json!({ "type": kind, "x": at.x, "y": at.y, "button": "left", "clickCount": 1 });
        self.call("Input.dispatchMouseEvent", mouse("mousePressed")).await.map_err(gone)?;
        self.call("Input.dispatchMouseEvent", mouse("mouseReleased")).await.map_err(gone)?;
        // Between down and up nothing can be asked, so where the press
        // landed is asked afterwards. A press that went to a new page has
        // nobody left to say; one that is known to have landed on something
        // else cannot be taken back, and nothing more is done on this page.
        let took = self.in_page("pressedInPage", &[]).await.unwrap_or(Value::Null);
        let elsewhere = [&took["down"], &took["click"]].into_iter().filter_map(Value::as_str).find(|on| *on != "it");
        if let Some(other) = elsewhere {
            return Err(Broken::Strayed(format!("a press meant for {} landed on something else (\"{other}\"), and the session is over", target.said)));
        }
        notes.push(format!("Pressed: {}", target.said));
        Ok(())
    }

    /// Types `text` into the field numbered `n`, or says in `refused` why not.
    async fn type_into(&mut self, n: u32, text: &str, enter: bool, refused: &mut Vec<String>, notes: &mut Vec<String>) -> Result<(), Broken> {
        let target = match self.target(n).await? {
            Ok(target) => target,
            Err(why) => {
                refused.push(why);
                return Ok(());
            }
        };
        if let Some(why) = typing_refused(&target.field, text) {
            refused.push(why.to_string());
            return Ok(());
        }
        // The caret is given, the field is looked at again against what
        // was judged, and the text is put in, all in one turn of script in
        // the page: `typeInPage` says why. A field that is no longer what
        // was judged is not typed into; what it has become is judged, and
        // if that may be typed into it is asked again, as that. Twice: a
        // field that is another each time it is looked at is left alone.
        let mut key = target.key;
        let mut tries = 0;
        let typed = loop {
            // Each asking is made once and not again: an answer that did
            // not come may be of a typing that did happen.
            let typed = match self.in_page("typeInPage", &[json!(n), json!(text), json!(key)]).await {
                Ok(typed) => typed,
                Err(CallError::Answered(why)) => {
                    refused.push(format!("the page did not say whether that was typed: {why}"));
                    return Ok(());
                }
                Err(e) => return Err(gone(e)),
            };
            if typed["changed"] != true {
                break typed;
            }
            let became: Field = serde_json::from_value(typed["field"].clone()).map_err(|e| Broken::Failed(format!("the page's answer could not be read: {e}")))?;
            if let Some(why) = typing_refused(&became, text) {
                refused.push(why.to_string());
                return Ok(());
            }
            tries += 1;
            if tries > 2 {
                refused.push("the page changes what that field is each time it is given the caret, so nothing was typed".to_string());
                return Ok(());
            }
            key = typed["key"].as_str().unwrap_or_default().to_string();
        };
        if let Some(why) = typed["error"].as_str() {
            refused.push(why.to_string());
            return Ok(());
        }
        if typed["moved"] == true {
            refused.push("the page moved the caret away from that field, so nothing was typed".to_string());
            return Ok(());
        }
        if typed["strayed"].as_u64().unwrap_or_default() > 0 {
            let undone = if typed["left"].as_u64().unwrap_or_default() == 0 { "it was taken out again" } else { "not all of it could be taken out again" };
            return Err(Broken::Strayed(format!("what was typed into {} went into another field as well; {undone}, and the session is over", target.said)));
        }
        if typed["typed"] != true {
            refused.push("that field did not take what was typed".to_string());
            return Ok(());
        }
        notes.push(format!("Typed into: {}", target.said));
        if enter {
            // Enter is a key, and a key goes to whatever holds the caret
            // when it is pressed. A page that has taken the caret elsewhere
            // since the typing gets no Enter there. This is asked and then
            // done, in two calls, so a page can still move the caret in
            // between: what is pressed there is Enter and nothing typed.
            if self.in_page_patiently("holdsCaretInPage", &[json!(n)]).await? != true {
                refused.push("the page took the caret from that field once it was typed into, so Enter was not pressed".to_string());
                return Ok(());
            }
            let key = |kind: &str| json!({ "type": kind, "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "nativeVirtualKeyCode": 13, "text": "\r" });
            self.call("Input.dispatchKeyEvent", key("keyDown")).await.map_err(gone)?;
            self.call("Input.dispatchKeyEvent", key("keyUp")).await.map_err(gone)?;
        }
        Ok(())
    }

    /// Chooses `option` in the list numbered `n`.
    async fn select(&mut self, n: u32, option: &str, refused: &mut Vec<String>, notes: &mut Vec<String>) -> Result<(), Broken> {
        let chose = self.in_page_patiently("chooseInPage", &[json!(n), json!(option)]).await?;
        match (chose["error"].as_str(), chose["chosen"].as_str()) {
            (Some(why), _) => refused.push(why.to_string()),
            // Said back as the page words it, since what was asked for
            // may have been only near it.
            (None, Some(chosen)) => notes.push(format!("Chosen: {chosen}")),
            (None, None) => {}
        }
        Ok(())
    }

    /// Goes back one page, if the page before is one a session may be on.
    async fn back(&mut self, refused: &mut Vec<String>) -> Result<(), Broken> {
        let history = self.call("Page.getNavigationHistory", json!({})).await.map_err(gone)?;
        let current = history["currentIndex"].as_u64().unwrap_or_default() as usize;
        // Going back is no request Chrome asks leave for, so where it
        // leads is read off the browser's record beforehand. The first
        // entry is the empty tab Chrome began with.
        let before = current.checked_sub(1).map(|index| &history["entries"][index]);
        let leads_to = before.and_then(|entry| Url::parse(entry["url"].as_str().unwrap_or_default()).ok());
        match (before, leads_to) {
            (Some(entry), Some(url)) if url.scheme() != "about" => match address_refused(&self.shop, &url) {
                Some(why) => refused.push(why.why().to_string()),
                None => {
                    self.call("Page.navigateToHistoryEntry", json!({ "entryId": entry["id"] })).await.map_err(gone)?;
                }
            },
            _ => refused.push("there is no page of this shop to go back to".to_string()),
        }
        Ok(())
    }

    /// Opens another address of the shop in the session's page.
    async fn open(&mut self, address: &str, refused: &mut Vec<String>) -> Result<(), Broken> {
        let url = match asked_address(address) {
            Ok(url) => url,
            Err(why) => {
                refused.push(why.why().to_string());
                return Ok(());
            }
        };
        if let Some(why) = address_refused(&self.shop, &url) {
            refused.push(why.why().to_string());
            return Ok(());
        }
        if !guard_allows(self.reaching, &url).await {
            refused.push(NOT_PUBLIC.to_string());
            return Ok(());
        }
        // Where this fails, the reason is already on its way: a redirect
        // refused is told by the one who refused it, and a page that did
        // not load is found, and left, when the page is next looked at.
        let _ = self.go(&url).await?;
        Ok(())
    }

    /// Reads the page afresh, which numbers everything on it anew, and
    /// shows it from the top, with what Chrome said in the meantime.
    async fn done(&mut self, refused: Vec<String>, notes: Vec<String>) -> Result<Done, Broken> {
        let caps = json!({ "title": TITLE_CHARS, "address": ADDRESS_CHARS, "markup": MARKUP_CHARS, "whole": OUTLINE_WHOLE });
        let page = self.in_page_patiently("pageInPage", &[caps]).await?;
        let text = |key: &str| page[key].as_str().unwrap_or_default().to_string();
        // Which document this is, to Chrome: the status it came with is
        // kept under that name.
        let tree = self.call("Page.getFrameTree", json!({})).await.map_err(gone)?;
        let loader = tree["frameTree"]["frame"]["loaderId"].as_str().unwrap_or_default().to_string();
        self.read = Some(Read { title: text("title"), url: text("url"), at: text("at"), html: text("html"), text: text("text"), cut: page["cut"] == true, loader });
        self.from = 0;
        self.shown(refused, notes)
    }

    /// The part at `self.from` of the page as it was last read, with what
    /// Chrome said in the meantime. Nothing is asked of the page, so
    /// nothing on it is numbered again.
    fn shown(&mut self, mut refused: Vec<String>, mut notes: Vec<String>) -> Result<Done, Broken> {
        let status = {
            let mut happened = self.shared.happened.lock().unwrap();
            if let Some(why) = happened.broke {
                return Err(Broken::Failed(why.into()));
            }
            for why in happened.refused.drain(..) {
                if !refused.contains(&why) {
                    refused.push(why);
                }
            }
            notes.append(&mut happened.notes);
            if happened.untold > 0 {
                notes.push(format!("and {} more that are not shown", happened.untold));
                happened.untold = 0;
            }
            let loader = self.read.as_ref().map(|read| read.loader.as_str());
            happened.statuses.iter().find(|(known, _)| Some(known.as_str()) == loader).map(|(_, status)| *status)
        };
        let Some(read) = &self.read else {
            return Err(Broken::Failed("the page was never read".into()));
        };
        let text: String = read.text.chars().skip(self.from).take(OUTLINE_CHARS).collect();
        let more = read.text.chars().nth(self.from + OUTLINE_CHARS).is_some();
        if !more && read.cut {
            notes.push(format!("The page goes on, and a session reads no more than {OUTLINE_WHOLE} characters of one."));
        }
        let seen = Seen { title: read.title.clone(), url: read.url.clone(), text, more, status, html: read.html.clone() };
        Ok(Done { refused, notes, seen })
    }
}

impl Page for Tab {
    async fn act(&mut self, action: &Action) -> Result<Done, Broken> {
        let mut refused = Vec::new();
        let mut notes = Vec::new();
        // A page can move itself between two actions; where it is now is
        // settled before anything is done to it.
        self.hold(&mut refused).await?;
        match action {
            Action::Read { more: false } => return self.done(refused, notes).await,
            Action::Read { more: true } => {
                // Reading on is more of the page as it was read, numbers
                // and all: read afresh, the page as it has become since
                // would be numbered anew, and a number the caller holds
                // would stand for something else.
                let still_there = match &self.read {
                    Some(read) => self.address().await? == read.at,
                    None => false,
                };
                if still_there {
                    self.from += OUTLINE_CHARS;
                    return self.shown(refused, notes);
                }
                notes.push("The page is no longer the one that was being read. This is the page as it is, from the top.".to_string());
                return self.done(refused, notes).await;
            }
            Action::Click { n } => self.click(*n, &mut refused, &mut notes).await?,
            Action::Type { n, text, enter } => self.type_into(*n, text, *enter, &mut refused, &mut notes).await?,
            Action::Select { n, option } => self.select(*n, option, &mut refused, &mut notes).await?,
            Action::Back => self.back(&mut refused).await?,
            Action::Open { url } => self.open(url, &mut refused).await?,
        }
        // Whatever was done, the page is read afresh once it has come to
        // rest and is where it may be.
        self.settle().await;
        self.hold(&mut refused).await?;
        self.done(refused, notes).await
    }

    async fn close(self) {
        self.watcher.abort();
        // Chrome first, and until it is gone; then its profile, until that
        // has stayed gone: `Profile` says why once is not enough.
        self.pipe.stop().await;
        self.profile.remove().await;
    }
}

impl Drop for Tab {
    /// A page that is let go of takes its Chrome with it. Without this the
    /// task that listens to the Chrome would keep the pipe open, and the
    /// Chrome running, for as long as the server did (found by a test: a
    /// session dropped unclosed left its Chrome alive).
    fn drop(&mut self) {
        self.watcher.abort();
        self.pipe.stop_now();
    }
}

/// What `watch` needs to know its own page by.
struct Watched {
    shop: Shop,
    session: String,
    frame: String,
    target: String,
}

/// Listens to Chrome for as long as it runs, and answers what cannot wait
/// for the session's next action: a request held for a verdict, a dialog,
/// another window.
async fn watch(pipe: Arc<Pipe>, mut said: mpsc::UnboundedReceiver<Event>, shared: Arc<Shared>, page: Watched) {
    while let Some(event) = said.recv().await {
        let ours = event.session.as_deref() == Some(page.session.as_str());
        let main = event.params["frameId"] == page.frame.as_str();
        let on = event.session.as_deref();
        match event.method.as_str() {
            "Fetch.requestPaused" => {
                let request = json!({ "requestId": event.params["requestId"] });
                let verdict = if !ours {
                    // Another window's, on its way to being closed.
                    Some(None)
                } else if main && event.params["resourceType"] == "Document" {
                    match Url::parse(event.params["request"]["url"].as_str().unwrap_or_default()) {
                        Ok(url) => address_refused(&page.shop, &url).map(|why| Some(why.why().to_string())),
                        Err(_) => Some(Some("that led to no address at all".to_string())),
                    }
                } else {
                    None
                };
                let sent = match verdict {
                    None => pipe.send("Fetch.continueRequest", request, on).await,
                    Some(why) => {
                        if let Some(why) = why {
                            shared.refuse(why);
                        }
                        // Aborted, and not blocked: an aborted navigation
                        // leaves the page where it was, where a blocked one
                        // replaces it with Chrome's page about the error.
                        let mut request = request;
                        request["errorReason"] = "Aborted".into();
                        pipe.send("Fetch.failRequest", request, on).await
                    }
                };
                if sent.is_err() {
                    return;
                }
            }
            "Network.responseReceived" if ours && main && event.params["type"] == "Document" => {
                let status = event.params["response"]["status"].as_u64().and_then(|status| u16::try_from(status).ok());
                if let (Some(loader), Some(status)) = (event.params["loaderId"].as_str(), status) {
                    let mut happened = shared.happened.lock().unwrap();
                    happened.statuses.retain(|(known, _)| known != loader);
                    happened.statuses.push((loader.to_string(), status));
                    let kept = happened.statuses.len().saturating_sub(STATUSES_KEPT);
                    happened.statuses.drain(..kept);
                }
            }
            "Page.frameStartedLoading" if ours && main => {
                shared.loading.send_replace(true);
            }
            "Page.frameStoppedLoading" if ours && main => {
                shared.loading.send_replace(false);
            }
            "Page.javascriptDialogOpening" if ours => {
                // An alert would hold every later call, and nobody is here
                // to read it. Dismissed, except the one that asks before
                // leaving, which would keep the session where it is.
                let leaving = event.params["type"] == "beforeunload";
                if !leaving {
                    let message: String = event.params["message"].as_str().unwrap_or_default().chars().take(200).collect();
                    shared.note(format!("The page put up a message, which was dismissed: \"{message}\""));
                }
                let _ = pipe.send("Page.handleJavaScriptDialog", json!({ "accept": leaving }), on).await;
            }
            "Target.attachedToTarget" => {
                let born = &event.params["targetInfo"];
                if born["type"] == "page" && born["targetId"] != page.target.as_str() {
                    shared.refuse("the page tried to open another window, and a session has one page".to_string());
                    let theirs = event.params["sessionId"].as_str();
                    // It is waiting to be let run, and the page that opened
                    // it waits with it (measured on Chrome 157: closed
                    // while held, the opener's own press was never
                    // answered). So it is let run with every request of its
                    // own held and failed above, and then closed.
                    let _ = pipe.call("Fetch.enable", json!({ "patterns": [{ "urlPattern": "*" }] }), theirs).await;
                    let _ = pipe.call("Runtime.runIfWaitingForDebugger", json!({}), theirs).await;
                    let _ = pipe.call("Target.closeTarget", json!({ "targetId": born["targetId"] }), None).await;
                }
            }
            "Page.fileChooserOpened" if ours => {
                shared.refuse("the page asked for a file, and a session uploads nothing".to_string());
            }
            "Browser.downloadWillBegin" => {
                shared.refuse("the page offered a file to save, and a session downloads nothing".to_string());
            }
            "Browser.downloadProgress" => {
                // Chrome was told to deny every download, and says here
                // that it did. Should one ever be saved all the same, the
                // session is not one to go on with.
                let state = event.params["state"].as_str().unwrap_or_default();
                let mut happened = shared.happened.lock().unwrap();
                match state {
                    "canceled" => happened.downloads_denied += 1,
                    "completed" => happened.broke = Some("a file was downloaded, which a session never does"),
                    _ => {}
                }
            }
            "Inspector.targetCrashed" if ours => {
                shared.happened.lock().unwrap().broke = Some("the page crashed");
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{self, html, Pages};

    /// A session on `path` of a test's own server, or `None` on a machine
    /// with no Chrome, after saying so.
    async fn open(pages: &Pages, path: &str) -> Option<(Tab, Done)> {
        let chrome = Chrome::reaching(testing::chrome_or_skip()?, pages.addr);
        let url = Url::parse(&pages.url(path)).unwrap();
        Some(chrome.open(&Shop::of(&url).unwrap(), &url).await.expect("the session opens"))
    }

    /// The page the prototype's own outline test read, less its script.
    const WIDGET: &str = r#"<!doctype html><title>Widget</title><body>
        <div id="cookie-banner"><p>Cookies?</p><button>Alleen noodzakelijk</button><button>Alles accepteren</button></div>
        <main>
          <h1>Widget</h1>
          <p class="price"><span>259</span><span>,-</span></p>
          <p class="price">€ <span>12</span><sup>,99</sup> per stuk</p>
          <p>Two <b>bold</b> words and a <a href="/next">link to press</a>.</p>
          <label for="pc">Postcode</label><input id="pc" name="postcode">
          <select aria-label="Maat"><option>S</option><option selected>M</option></select>
          <p style="display:none">hidden 999,99</p>
          <button aria-hidden="true">not for anyone</button>
        </main>
        <footer><a href="/about">Over ons</a></footer>
      </body>"#;

    #[tokio::test]
    async fn a_session_opens_on_a_page_and_is_shown_its_outline() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[("/p/widget", html(WIDGET))]).await;
        let Some((tab, done)) = open(&pages, "/p/widget").await else { return };
        let seen = &done.seen;
        assert_eq!(done.refused, Vec::<String>::new());
        assert_eq!(seen.title, "Widget");
        assert_eq!(seen.url, pages.url("/p/widget"));
        assert_eq!(seen.status, Some(200));
        assert!(!seen.more);
        // An amount the page writes in pieces is kept whole.
        assert!(seen.text.contains("259,-"), "{}", seen.text);
        assert!(seen.text.contains("12,99"), "{}", seen.text);
        assert!(seen.text.contains("Two bold words and a"), "{}", seen.text);
        assert!(!seen.text.contains("999,99") && !seen.text.contains("not for anyone"), "{}", seen.text);
        // What sits in front of the page comes first, then the page, then what is around it.
        let at = |needle: &str| seen.text.find(needle).unwrap_or_else(|| panic!("{needle} in {}", seen.text));
        assert!(at("Alleen noodzakelijk") < at("Widget") && at("Widget") < at("Over ons"), "{}", seen.text);
        // The things that can be pressed are numbered, in the order they are read.
        for numbered in [
            r#"[1] button "Alleen noodzakelijk""#,
            r#"[2] button "Alles accepteren""#,
            r#"[3] link "link to press""#,
            r#"[4] field "Postcode""#,
            r#"[5] list "Maat": S | *M"#,
            r#"[6] link "Over ons""#,
        ] {
            assert!(seen.text.contains(numbered), "{numbered} in {}", seen.text);
        }
        // The markup rides along for the one who judges a refusal, and is the page's own.
        assert!(seen.html.contains(r#"<p style="display:none">hidden 999,99</p>"#), "{}", seen.html);
        assert_eq!(pages.asked(), ["GET /p/widget"]);
        tab.close().await;
    }

    #[tokio::test]
    async fn a_long_page_is_read_in_parts_of_6000_characters() {
        let _one_of_few = testing::gate().await;
        let lines: String = (1..=400).map(|n| format!("<p>Line {n:03} of the widget's description.</p>")).collect();
        let pages = Pages::serve(&[("/long", html(&format!("<title>Long</title><main>{lines}<a href='/end'>the last link</a></main>")))]).await;
        let Some((mut tab, first)) = open(&pages, "/long").await else { return };
        assert!(first.seen.more);
        assert_eq!(first.seen.text.chars().count(), OUTLINE_CHARS);
        assert!(first.seen.text.contains("Line 001") && !first.seen.text.contains("Line 400"));

        let next = tab.act(&Action::Read { more: true }).await.unwrap();
        assert!(!next.seen.text.contains("Line 001"));
        assert!(next.seen.text.chars().count() <= OUTLINE_CHARS);
        // Read on to the end: the parts join without a gap or a repeat.
        let mut whole = first.seen.text.clone() + &next.seen.text;
        let mut last = next;
        while last.seen.more {
            last = tab.act(&Action::Read { more: true }).await.unwrap();
            whole += &last.seen.text;
        }
        for n in 1..=400 {
            assert_eq!(whole.matches(&format!("Line {n:03} of")).count(), 1, "line {n}");
        }
        assert!(whole.ends_with(r#"link "the last link""#), "{}", &whole[whole.len() - 80..]);
        // A plain read starts again at the top.
        let again = tab.act(&Action::Read { more: false }).await.unwrap();
        assert_eq!(again.seen.text, first.seen.text);
        tab.close().await;
    }

    #[tokio::test]
    async fn a_session_on_an_address_the_guard_refuses_never_starts() {
        // No test server is let through here, so loopback is what it is to
        // the guard. No Chrome is needed to be told no.
        let chrome = Chrome::new(PathBuf::from("/definitely/not/chrome"));
        for address in ["http://127.0.0.1:9090/metrics", "http://localhost:9090/", "http://169.254.169.254/latest/meta-data/", "http://10.0.0.1/"] {
            let url = Url::parse(address).unwrap();
            let refused = chrome.open(&Shop::of(&url).unwrap(), &url).await.err();
            assert_eq!(refused, Some(Broken::Refused(NOT_PUBLIC.to_string())), "{address}");
        }
    }

    #[tokio::test]
    async fn a_session_runs_at_the_pace_the_reading_ran_at_unless_a_test_says_otherwise() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[("/p/widget", html(WIDGET)), ("/next", html("<title>Next</title><main>the next page</main>"))]).await;
        let Some(path) = testing::chrome_or_skip() else { return };
        assert_eq!(Chrome::new(path.clone()).pace, Pace::MEASURED);
        let chrome = Chrome { pace: Pace::MEASURED, ..Chrome::reaching(path, pages.addr) };
        let url = Url::parse(&pages.url("/p/widget")).unwrap();
        let (mut tab, _) = chrome.open(&Shop::of(&url).unwrap(), &url).await.unwrap();
        let pressed = std::time::Instant::now();
        let done = tab.act(&Action::Click { n: 3 }).await.unwrap();
        assert_eq!(done.seen.title, "Next");
        // 400 ms for the press to begin, 900 ms for the page's scripts.
        let took = pressed.elapsed();
        assert!(took >= Duration::from_millis(1300) && took < Duration::from_secs(6), "{took:?}");
        tab.close().await;
    }

    /// Whether `profile` is gone, and stays gone: a Chrome still alive when
    /// its profile is removed writes a file or two back into it.
    async fn gone_for_good(profile: &std::path::Path) -> bool {
        for _ in 0..10 {
            if profile.exists() {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        true
    }

    #[tokio::test]
    async fn a_session_that_is_closed_leaves_nothing_of_its_profile() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[("/p/widget", html(WIDGET))]).await;
        let Some((tab, _)) = open(&pages, "/p/widget").await else { return };
        let profile = tab.profile.path().to_path_buf();
        assert!(profile.join("Default").exists(), "Chrome keeps its profile where it was told to");
        tab.close().await;
        assert!(gone_for_good(&profile).await, "{} is still there", profile.display());
    }

    #[tokio::test]
    async fn a_session_that_is_dropped_unclosed_leaves_nothing_either() {
        // A call that is given up halfway, or a server on its way down,
        // does not get to close anything: the page is simply let go of.
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[("/p/widget", html(WIDGET))]).await;
        let Some((tab, _)) = open(&pages, "/p/widget").await else { return };
        let profile = tab.profile.path().to_path_buf();
        let chrome = tab.pipe.chrome_process().await.expect("a Chrome is running");
        assert!(is_running(chrome));
        drop(tab);
        // The Chrome is gone by the time the page is: nothing is left to
        // wait for that a caller who has let go would never wait for.
        assert!(!is_running(chrome), "Chrome {chrome} outlived its session");
        assert!(gone_for_good(&profile).await, "{} is still there", profile.display());
    }

    /// Whether a process is there to be signalled.
    fn is_running(process: u32) -> bool {
        std::process::Command::new("kill").args(["-0", &process.to_string()]).stderr(std::process::Stdio::null()).status().is_ok_and(|status| status.success())
    }

    // ------------------------------------------------------------ a press

    use crate::address::Refused;
    use crate::testing::{attachment, redirect, status};

    const NOTHING: Vec<String> = Vec::new();

    /// A page called Home with `body` in it, at `/`.
    fn home(body: &str) -> (&'static str, testing::Reply) {
        ("/", html(&format!("<!doctype html><title>Home</title><main><h1>Home</h1>{body}</main>")))
    }

    fn page(path: &'static str, title: &str) -> (&'static str, testing::Reply) {
        (path, html(&format!("<!doctype html><title>{title}</title><main><h1>{title}</h1></main>")))
    }

    async fn press(tab: &mut Tab, n: u32) -> Done {
        tab.act(&Action::Click { n }).await.expect("the browser answers")
    }

    #[tokio::test]
    async fn a_press_follows_a_link_to_another_page_of_the_shop() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(r#"<a href="/next">the next page</a>"#), page("/next", "Next")]).await;
        let Some((mut tab, first)) = open(&pages, "/").await else { return };
        assert!(first.seen.text.contains(r#"[1] link "the next page""#), "{}", first.seen.text);
        let done = press(&mut tab, 1).await;
        assert_eq!(done.refused, NOTHING);
        assert_eq!((done.seen.title.as_str(), done.seen.url.as_str()), ("Next", pages.url("/next").as_str()));
        assert_eq!(pages.asked(), ["GET /", "GET /next"]);
        tab.close().await;
    }

    #[tokio::test]
    async fn a_link_that_leaves_the_shop_is_refused_and_not_pressed() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(r#"<a href="https://other.example/x">another shop</a>"#)]).await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        let done = press(&mut tab, 1).await;
        assert_eq!(done.refused, [Refused::LeavesShop.why()]);
        assert_eq!((done.seen.title.as_str(), done.seen.url.as_str()), ("Home", pages.url("/").as_str()));
        tab.close().await;
    }

    #[tokio::test]
    async fn a_redirect_off_the_shop_is_refused_at_the_hop_and_the_page_stays() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[
            home(r#"<a href="/out">a way out</a> <a href="/buy">buy it now</a>"#),
            ("/out", redirect("https://other.example/landed")),
            // And one that stays on the shop and ends in its checkout.
            ("/buy", redirect("{base}/hop")),
            ("/hop", redirect("{base}/checkout/address")),
            page("/checkout/address", "Checkout"),
        ])
        .await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        let done = press(&mut tab, 1).await;
        assert_eq!(done.refused, [Refused::LeavesShop.why()]);
        assert_eq!((done.seen.title.as_str(), done.seen.url.as_str()), ("Home", pages.url("/").as_str()));
        // The first hop was the shop's and was made; the second never was.
        assert!(pages.was_asked("/out"));

        let done = press(&mut tab, 2).await;
        assert_eq!(done.refused, [Refused::Checkout.why()]);
        assert_eq!(done.seen.title, "Home");
        assert!(pages.was_asked("/buy") && pages.was_asked("/hop"));
        assert!(!pages.was_asked("/checkout/address"), "{:?}", pages.asked());
        tab.close().await;
    }

    #[tokio::test]
    async fn a_script_that_sends_the_page_off_the_shop_is_refused() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[
            home(
                r#"<button onclick="location.href = 'https://other.example/by-script'">leave by script</button>
                   <button onclick="location.href = '/account/login'">sign in by script</button>
                   <form action="https://other.example/form"><button>leave by a form</button></form>"#,
            ),
            page("/account/login", "Sign in"),
        ])
        .await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        let done = press(&mut tab, 1).await;
        assert_eq!(done.refused, [Refused::LeavesShop.why()]);
        assert_eq!(done.seen.title, "Home");
        let done = press(&mut tab, 2).await;
        assert_eq!(done.refused, [Refused::SignIn.why()]);
        assert_eq!(done.seen.title, "Home");
        assert!(!pages.was_asked("/account/login"));
        let done = press(&mut tab, 3).await;
        assert_eq!(done.refused, [Refused::LeavesShop.why()]);
        assert_eq!(done.seen.title, "Home");
        tab.close().await;
    }

    #[tokio::test]
    async fn a_checkout_or_a_sign_in_link_is_refused_before_anything_is_asked_of_the_shop() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[
            home(r#"<a href="/checkout">Afrekenen</a> <a href="/account/login">Inloggen</a> <a href="/cart">Winkelwagen</a>"#),
            page("/checkout", "Checkout"),
            page("/account/login", "Sign in"),
            page("/cart", "Basket"),
        ])
        .await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        assert_eq!(press(&mut tab, 1).await.refused, [Refused::Checkout.why()]);
        assert_eq!(press(&mut tab, 2).await.refused, [Refused::SignIn.why()]);
        assert_eq!(pages.asked(), ["GET /"]);
        // The basket is as far as a session goes, and it goes there.
        let done = press(&mut tab, 3).await;
        assert_eq!((done.refused, done.seen.title.as_str()), (NOTHING, "Basket"));
        tab.close().await;
    }

    #[tokio::test]
    async fn a_checkout_reached_without_a_request_is_left_again() {
        let _one_of_few = testing::gate().await;
        let became = "document.title = 'Checkout'; document.querySelector('h1').textContent = 'Your order'";
        let pages = Pages::serve(&[home(&format!(
            r#"<button onclick="history.pushState({{}}, '', '/checkout'); {became}">by a written address</button>
               <button onclick="location.hash = '/checkout'; {became}">by a route after the hash</button>"#
        ))])
        .await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        for n in [1, 2] {
            let done = press(&mut tab, n).await;
            assert_eq!(done.refused, [format!("{}; the session went back to the page it was on", Refused::Checkout.why())], "press {n}");
            assert_eq!((done.seen.title.as_str(), done.seen.url.as_str()), ("Home", pages.url("/").as_str()), "press {n}");
            assert!(!done.seen.text.contains("Your order"), "{}", done.seen.text);
        }
        // No request for the checkout was ever made: there was none to refuse.
        assert!(!pages.was_asked("/checkout"));
        tab.close().await;
    }

    #[tokio::test]
    async fn nothing_is_downloaded() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(r#"<a href="/manual.bin">download the manual</a>"#), ("/manual.bin", attachment("widget-manual.bin", "not a manual"))]).await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        let done = press(&mut tab, 1).await;
        assert_eq!(done.refused, ["the page offered a file to save, and a session downloads nothing"]);
        assert_eq!((done.seen.title.as_str(), done.seen.url.as_str()), ("Home", pages.url("/").as_str()));
        // Chrome says what became of the file, a moment after it says one
        // was offered: stopped, and the session is still one to go on with.
        for _ in 0..50 {
            if tab.shared.happened.lock().unwrap().downloads_denied > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(tab.shared.happened.lock().unwrap().downloads_denied, 1);
        assert_eq!(tab.act(&Action::Read { more: false }).await.map(|done| done.seen.title), Ok("Home".to_string()));
        tab.close().await;
    }

    #[tokio::test]
    async fn a_link_that_would_start_another_program_is_not_pressed() {
        let _one_of_few = testing::gate().await;
        // Chrome is never asked for these, so nothing that judges requests
        // would see them: the link is read before it is pressed.
        let pages = Pages::serve(&[home(r#"<a href="mailto:?subject=widget">write to us</a> <a href="tel:0000">call us</a>"#)]).await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        for n in [1, 2] {
            let done = press(&mut tab, n).await;
            assert_eq!(done.refused, [Refused::NotWeb.why()]);
            assert_eq!(done.seen.title, "Home");
        }
        tab.close().await;
    }

    #[tokio::test]
    async fn nothing_is_uploaded() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(
            r#"<label>A photo of the widget <input type="file" name="photo"></label>
               <button onclick="document.querySelector('input').click()">choose a file</button>"#,
        )])
        .await;
        let Some((mut tab, first)) = open(&pages, "/").await else { return };
        assert!(first.seen.text.contains(r#"[1] file "A photo of the widget""#), "{}", first.seen.text);
        // The field itself is not pressed.
        assert_eq!(press(&mut tab, 1).await.refused, ["that is for choosing a file, and a session uploads nothing"]);
        // A button that opens the chooser by script gets no chooser and no file.
        let done = press(&mut tab, 2).await;
        assert_eq!(done.refused, ["the page asked for a file, and a session uploads nothing"]);
        assert_eq!(done.seen.title, "Home");
        tab.close().await;
    }

    #[tokio::test]
    async fn another_window_is_closed_before_it_has_fetched_anything() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[
            home(
                r#"<button onclick="const w = window.open('/popup'); document.querySelector('h1').textContent = 'Home, still'">in a window of its own</button>
                   <a href="/elsewhere" target="_blank">in a new tab</a>"#,
            ),
            page("/popup", "Popup"),
            page("/elsewhere", "Elsewhere"),
        ])
        .await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        let mut done = press(&mut tab, 1).await;
        // The session is told why no window came, and the page that asked
        // for one goes on running: both once Chrome has said so and the
        // window is closed, which is not always before the press is
        // answered. (Seen once: the press came back with the refusal and
        // with the page as it stood before its script went on.)
        let mut refused = std::mem::take(&mut done.refused);
        for _ in 0..30 {
            if !refused.is_empty() && done.seen.text.contains("Home, still") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            done = act(&mut tab, Action::Read { more: false }).await;
            refused.append(&mut done.refused);
        }
        assert_eq!(refused, ["the page tried to open another window, and a session has one page"]);
        assert_eq!(done.seen.title, "Home");
        assert!(done.seen.text.contains("Home, still"), "{}", done.seen.text);
        assert!(!pages.was_asked("/popup"), "{:?}", pages.asked());
        // A link that asks for a tab of its own is followed in the session's one page.
        let done = press(&mut tab, 2).await;
        assert_eq!((done.refused, done.seen.title.as_str()), (NOTHING, "Elsewhere"));
        tab.close().await;
    }

    #[tokio::test]
    async fn a_permission_the_page_asks_for_is_refused_without_a_prompt() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(
            r#"<button onclick="ask()">where am I</button><p id="answer">not asked</p>
               <script>
                 async function ask() {
                   const notify = await Notification.requestPermission()
                   const where = await new Promise((said) => navigator.geolocation.getCurrentPosition(() => said('given'), (e) => said('refused, code ' + e.code)))
                   document.getElementById('answer').textContent = 'notifications ' + notify + '; location ' + where
                 }
               </script>"#,
        )])
        .await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        let mut done = press(&mut tab, 1).await;
        // The answer comes when Chrome gives it, which is not when the page settles.
        for _ in 0..50 {
            if !done.seen.text.contains("not asked") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            done = tab.act(&Action::Read { more: false }).await.unwrap();
        }
        // Code 1 is PERMISSION_DENIED.
        assert!(done.seen.text.contains("notifications denied; location refused, code 1"), "{}", done.seen.text);
        tab.close().await;
    }

    #[tokio::test]
    async fn a_press_lands_on_the_thing_numbered_or_not_at_all() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(
            r#"<button onclick="document.querySelector('h1').textContent = 'the button was pressed'">the button</button>
               <div style="position: fixed; inset: 0; background: rgba(0, 0, 0, .5)"
                    onclick="document.querySelector('h1').textContent = 'the overlay took the press'">Subscribe first</div>"#,
        )])
        .await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        let done = press(&mut tab, 1).await;
        assert_eq!(done.refused, [r#"something lies over that and would take the press: "Subscribe first""#]);
        assert!(done.seen.text.contains("Home") && !done.seen.text.contains("took the press") && !done.seen.text.contains("was pressed"), "{}", done.seen.text);
        tab.close().await;
    }

    #[tokio::test]
    async fn a_link_that_runs_over_two_lines_is_pressed_where_it_is() {
        let _one_of_few = testing::gate().await;
        // The middle of this link's box is on the second line, to the right
        // of where the link ends: a press there lands on the paragraph.
        let pages = Pages::serve(&[
            home(r#"<p style="width: 300px; margin: 0; font: 16px/20px monospace">xxxxxxxxxxxxxxxxxxxx <a href="/next">aaaaaaa bbbb</a></p>"#),
            page("/next", "Next"),
        ])
        .await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        let done = press(&mut tab, 1).await;
        assert_eq!((done.refused, done.seen.title.as_str()), (NOTHING, "Next"));
        tab.close().await;
    }

    #[tokio::test]
    async fn a_number_that_is_not_on_the_page_is_said_to_be_so() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(r#"<a href="/next">the next page</a>"#)]).await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        let done = press(&mut tab, 99).await;
        assert_eq!(done.refused, ["nothing is numbered 99 on this page now; read it again"]);
        assert_eq!(pages.asked(), ["GET /"]);
        tab.close().await;
    }

    #[tokio::test]
    async fn a_message_the_page_puts_up_is_dismissed_and_passed_on() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(r#"<button onclick="alert('Uitverkocht'); document.querySelector('h1').textContent = 'after the message'">in de winkelwagen</button>"#)]).await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        let done = press(&mut tab, 1).await;
        assert_eq!(done.notes, [r#"Pressed: button "in de winkelwagen""#, r#"The page put up a message, which was dismissed: "Uitverkocht""#]);
        assert!(done.seen.text.contains("after the message"), "{}", done.seen.text);
        assert_eq!(done.refused, NOTHING);
        tab.close().await;
    }

    #[tokio::test]
    async fn the_guard_stands_between_a_session_and_the_rest_of_loopback() {
        let _one_of_few = testing::gate().await;
        // A second server, which no test lets a session through to: to the
        // guard it is what a pod's own metrics listener is.
        let neighbour = Pages::serve(&[("/secret", html("<title>Secret</title>secret_total 1")), ("/pixel", html("x"))]).await;
        let pages = Pages::serve(&[home(&format!(
            r#"<a href="http://{0}/secret">a neighbour</a><img src="http://{0}/pixel">
               <script>fetch('http://{0}/secret').catch(() => {{}})</script>"#,
            neighbour.addr
        ))])
        .await;
        let Some((mut tab, first)) = open(&pages, "/").await else { return };
        assert_eq!(first.seen.title, "Home");
        // The same host by number, so the one-shop rule lets the link be
        // pressed. What stops it is the proxy every connection goes through.
        let done = press(&mut tab, 1).await;
        assert_eq!(done.refused, ["that page could not be loaded; the session went back to the page it was on"]);
        assert_eq!((done.seen.title.as_str(), done.seen.url.as_str()), ("Home", pages.url("/").as_str()));
        assert!(!done.seen.text.contains("secret_total"), "{}", done.seen.text);
        assert_eq!(neighbour.asked(), NOTHING, "neither the link, the picture nor the script reached it");
        tab.close().await;
    }

    // ------------------------------- what a number stands for, and what is seen

    #[tokio::test]
    async fn a_number_means_what_it_meant_when_the_page_was_read_on() {
        let _one_of_few = testing::gate().await;
        // A page that puts a button in front of everything a moment after
        // it is first read. Numbered again at every read, 1 became that
        // button, and "press 1" after reading on pressed it.
        let lines: String = (1..=400).map(|n| format!("<p>Line {n:03} of the widget's description.</p>")).collect();
        let pages = Pages::serve(&[
            home(&format!(
                r#"<a href="/next">the next page</a>{lines}
                   <script>
                     setTimeout(() => {{
                       const button = document.createElement('button')
                       button.textContent = 'Nu kopen'
                       button.onclick = () => {{ document.title = 'Bought' }}
                       document.querySelector('main').prepend(button)
                     }}, 1000)
                   </script>"#
            )),
            page("/next", "Next"),
        ])
        .await;
        let Some((mut tab, first)) = open(&pages, "/").await else { return };
        assert!(first.seen.text.contains(r#"[1] link "the next page""#), "{}", &first.seen.text[..200]);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let on = tab.act(&Action::Read { more: true }).await.unwrap();
        // Reading on goes on where the first part stopped, in the page as it was read.
        let joined = first.seen.text.clone() + &on.seen.text;
        assert!(!on.seen.text.contains("Nu kopen") && joined.matches("Line 158 of").count() == 1, "{}", &on.seen.text[..200]);
        let done = press(&mut tab, 1).await;
        assert_eq!((done.seen.title.as_str(), done.refused), ("Next", NOTHING));
        // And every press says what it pressed, so a caller can see it was what it meant.
        assert_eq!(done.notes, [r#"Pressed: link "the next page""#]);
        tab.close().await;
    }

    #[tokio::test]
    async fn a_press_that_something_slides_under_is_not_made() {
        let _one_of_few = testing::gate().await;
        // The pointer arriving is itself something a page hears, before the press.
        let pages = Pages::serve(&[home(
            r#"<button onmouseover="document.getElementById('other').style.cssText = 'position: fixed; inset: 0; display: block'"
                       onclick="document.title = 'The button was pressed'">the button</button>
               <button id="other" style="display: none" onclick="document.title = 'Nu kopen was pressed'">Nu kopen</button>"#,
        )])
        .await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        let done = press(&mut tab, 1).await;
        assert_eq!(done.refused, [r#"something came to lie over that as the pointer reached it, and would take the press: "Nu kopen""#]);
        assert_eq!(done.seen.title, "Home");
        tab.close().await;
    }

    #[tokio::test]
    async fn a_press_that_lands_elsewhere_all_the_same_is_the_end_of_the_session() {
        let _one_of_few = testing::gate().await;
        // Between the pointer going down and coming up there is no asking
        // the page anything: what can be done is to know afterwards.
        let pages = Pages::serve(&[home(
            r#"<button onmousedown="document.getElementById('other').style.cssText = 'position: fixed; inset: 0; display: block'">the button</button>
               <button id="other" style="display: none">Nu kopen</button>"#,
        )])
        .await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        let broken = tab.act(&Action::Click { n: 1 }).await.unwrap_err();
        assert!(matches!(&broken, Broken::Strayed(why) if why.contains(r#"button "the button""#) && why.contains("landed on something else")), "{broken:?}");
        tab.close().await;
    }

    #[tokio::test]
    async fn a_thing_that_only_wraps_a_frame_is_not_numbered() {
        let _one_of_few = testing::gate().await;
        // What is in a frame is not read, so a press that would go into
        // one presses something nobody was shown.
        let pages = Pages::serve(&[home(
            r#"<div role="button" aria-label="Volgende" style="width: 300px; height: 100px"><iframe style="width: 300px; height: 100px; border: 0"
                 srcdoc="<button style='width: 300px; height: 100px' onclick='parent.document.title = &quot;The frame was pressed&quot;'>Betaal nu</button>"></iframe></div>
               <button onclick="document.title = 'The button was pressed'">the button</button>"#,
        )])
        .await;
        let Some((mut tab, first)) = open(&pages, "/").await else { return };
        assert!(!first.seen.text.contains("Volgende"), "{}", first.seen.text);
        assert!(first.seen.text.contains(r#"[1] button "the button""#), "{}", first.seen.text);
        assert_eq!(press(&mut tab, 1).await.seen.title, "The button was pressed");
        tab.close().await;
    }

    #[tokio::test]
    async fn what_a_page_hides_is_neither_read_nor_numbered() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(
            r#"<p>Seen text.</p>
               <span style="position: absolute; left: -9999px">Text put off the page.</span>
               <span style="opacity: 0">Text made transparent.</span>
               <span style="font-size: 0">Text of no size.</span>
               <span style="position: absolute; width: 1px; height: 1px; overflow: hidden; clip: rect(0 0 0 0)">Text cut down to a dot.</span>
               <a href="/trap" style="position: absolute; width: 0; height: 0; overflow: hidden">A link of no size</a>
               <a href="/trap" style="opacity: 0">A link made transparent</a>
               <label style="display: inline-block; padding: 8px">
                 <input type="checkbox" name="gift" style="position: absolute; opacity: 0; width: 0; height: 0"> Als cadeau inpakken
               </label>
               <a href="/next">A link that is there</a>"#,
        )])
        .await;
        let Some((mut tab, first)) = open(&pages, "/").await else { return };
        let text = &first.seen.text;
        assert!(text.contains("Seen text."), "{text}");
        for hidden in ["off the page", "transparent", "no size", "a dot"] {
            assert!(!text.contains(hidden), "{hidden:?} in {text}");
        }
        // A tick box drawn by its label is seen by its label, and is pressed there.
        assert!(text.contains(r#"[1] checkbox "Als cadeau inpakken""#) && text.contains(r#"[2] link "A link that is there""#), "{text}");
        let done = press(&mut tab, 1).await;
        assert!(done.seen.text.contains(r#"[1] checkbox (on) "Als cadeau inpakken""#), "{}", done.seen.text);
        assert_eq!(done.notes, [r#"Pressed: checkbox "Als cadeau inpakken""#]);
        tab.close().await;
    }

    #[tokio::test]
    async fn what_a_password_field_holds_is_not_read_back() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(
            r#"<label>Wachtwoord <input type="password" name="pw" value="geheim-van-de-winkel"></label>
               <label>Kortingscode <input type="text" name="code" value="WIDGET10"></label>"#,
        )])
        .await;
        let Some((tab, first)) = open(&pages, "/").await else { return };
        // What an ordinary field holds is shown, which is how the other is known to be kept back.
        assert!(first.seen.text.contains(r#"[2] field "Kortingscode" = "WIDGET10""#), "{}", first.seen.text);
        assert!(first.seen.text.contains(r#"[1] field "Wachtwoord""#) && !first.seen.text.contains("geheim"), "{}", first.seen.text);
        tab.close().await;
    }

    #[tokio::test]
    async fn a_letter_in_two_parts_is_never_cut_in_half() {
        let _one_of_few = testing::gate().await;
        // A link's text is cut at 120, and a picture letter lies across the
        // cut. Cut by the half, the answer was no JSON, was dropped unread,
        // and the session died half a minute later of a browser "not answering".
        let link = format!("{}\u{1F600} en de rest van de naam", "a".repeat(119));
        let pages = Pages::serve(&[
            home(&format!(
                r#"<a href="/next">{link}</a><select aria-label="Smaak"><option>{link}</option></select>
                   <label>{link} <input name="q" value="{link}"></label>"#
            )),
            page("/next", "Next"),
        ])
        .await;
        let opening = std::time::Instant::now();
        let Some((mut tab, first)) = open(&pages, "/").await else { return };
        assert!(opening.elapsed() < Duration::from_secs(15), "{:?}", opening.elapsed());
        // The letter that does not fit whole is left out whole.
        assert!(first.seen.text.contains(&format!("[1] link \"{}\" ", "a".repeat(119))), "{}", first.seen.text);
        assert!(!first.seen.text.contains('\u{fffd}'), "a letter was cut in half: {}", first.seen.text);
        // And what a list answers a wrong choice with is cut the same way.
        let done = act(&mut tab, choose(2, "geen")).await;
        assert!(!done.refused[0].contains('\u{fffd}'), "{:?}", done.refused);
        tab.close().await;
    }

    #[tokio::test]
    async fn half_a_letter_of_the_pages_own_making_is_read_as_a_mark() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(
            r#"<p id="p"></p><button onclick="alert('uit\ud83d')">koop</button>
               <script>document.title = 'Widget \ud83d'; document.getElementById('p').textContent = 'halve \ude00 letter'</script>"#,
        )])
        .await;
        let Some((mut tab, first)) = open(&pages, "/").await else { return };
        assert_eq!(first.seen.title, "Widget \u{fffd}");
        assert!(first.seen.text.contains("halve \u{fffd} letter"), "{}", first.seen.text);
        // A message with half a letter in it is still dismissed, or the page would be held for good.
        let done = press(&mut tab, 1).await;
        assert!(done.notes.iter().any(|note| note.contains("uit\u{fffd}")), "{:?}", done.notes);
        tab.close().await;
    }

    #[tokio::test]
    async fn what_a_page_can_make_long_is_cut_to_size() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[
            home(
                r#"<select aria-label="Smaak"><option>kip</option><option>zalm</option></select>
                   <script>
                     document.title = 'T'.repeat(200000)
                     history.replaceState({}, '', '/?' + 'q'.repeat(100000))
                     document.querySelector('option').textContent = 'o'.repeat(300000)
                     document.querySelector('select').setAttribute('aria-label', 'L'.repeat(100000))
                   </script>"#,
            ),
            ("/heavy", html(&format!("<!doctype html><title>Heavy</title><main><h1>Heavy</h1><!-- {} --></main>", "x".repeat(3_000_000)))),
        ])
        .await;
        let Some((mut tab, first)) = open(&pages, "/").await else { return };
        assert_eq!(first.seen.title.chars().count(), TITLE_CHARS);
        assert_eq!(first.seen.url.chars().count(), ADDRESS_CHARS);
        // A list's name and its options are a few words each, whatever the page wrote.
        let list = first.seen.text.lines().find(|line| line.contains("[1] list")).expect("the list is numbered");
        assert!(list.chars().count() < 300, "{} characters", list.chars().count());
        let done = act(&mut tab, choose(1, "rund")).await;
        assert!(done.refused[0].chars().count() < 300, "{} characters", done.refused[0].chars().count());
        // A page of three million characters is judged by its first two million and is still a page.
        let heavy = act(&mut tab, go_to(&pages.url("/heavy"))).await;
        assert_eq!((heavy.seen.title.as_str(), heavy.seen.html.len()), ("Heavy", MARKUP_CHARS));
        tab.close().await;
    }

    #[tokio::test]
    async fn messages_without_end_are_told_as_a_few_and_a_count() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(r#"<script>setInterval(() => alert('x'.repeat(500)), 1)</script>"#)]).await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        tokio::time::sleep(Duration::from_secs(2)).await;
        let done = tab.act(&Action::Read { more: false }).await.unwrap();
        assert_eq!(done.notes.len(), NOTES_KEPT + 1, "{:?}", done.notes.iter().map(String::len).collect::<Vec<_>>());
        assert!(done.notes.iter().all(|note| note.chars().count() < 300));
        assert!(done.notes[NOTES_KEPT].starts_with("and ") && done.notes[NOTES_KEPT].ends_with(" more that are not shown"), "{}", done.notes[NOTES_KEPT]);
        tab.close().await;
    }

    #[tokio::test]
    async fn a_page_cannot_send_a_datagram_past_the_guard() {
        let _one_of_few = testing::gate().await;
        // The guard's proxy carries TCP. A page that asks the browser for a
        // peer connection asks it to send UDP, which no such proxy carries.
        let listening = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = listening.local_addr().unwrap().port();
        let pages = Pages::serve(&[home(&format!(
            r#"<p id="ice">gathering</p>
               <script>
                 (async () => {{
                   const said = document.getElementById('ice')
                   const peer = new RTCPeerConnection({{ iceServers: [{{ urls: 'stun:127.0.0.1:{port}' }}] }})
                   peer.createDataChannel('x')
                   peer.onicecandidate = (e) => {{ said.textContent += e.candidate ? ' candidate' : ' done' }}
                   await peer.setLocalDescription(await peer.createOffer())
                 }})()
               </script>"#
        ))])
        .await;
        let Some((mut tab, mut done)) = open(&pages, "/").await else { return };
        let mut datagram = [0u8; 1500];
        let mut arrived = None;
        for _ in 0..150 {
            arrived = tokio::time::timeout(Duration::from_millis(100), listening.recv_from(&mut datagram)).await.ok();
            if arrived.is_some() || done.seen.text.contains(" done") {
                break;
            }
            done = tab.act(&Action::Read { more: false }).await.unwrap();
        }
        assert!(arrived.is_none(), "a datagram reached 127.0.0.1:{port}, past the proxy: {arrived:?}");
        // The page is told there is no way out, at once and without a candidate.
        assert!(done.seen.text.contains("gathering done"), "{}", done.seen.text);
        tab.close().await;
    }

    #[tokio::test]
    async fn a_page_cannot_send_a_datagram_to_a_peer_it_names() {
        let _one_of_few = testing::gate().await;
        // No server is named here, so a setting that only kept Chrome from
        // asking servers would leave this open: the page tells one of its
        // own connections that its peer is at an address of the page's
        // choosing, and Chrome sends a check there to see.
        let listening = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = listening.local_addr().unwrap().port();
        let pages = Pages::serve(&[home(&format!(
            r#"<p id="peer">pairing</p>
               <script>
                 (async () => {{
                   const said = document.getElementById('peer')
                   try {{
                     const a = new RTCPeerConnection(), b = new RTCPeerConnection()
                     a.createDataChannel('x')
                     const offer = await a.createOffer()
                     await a.setLocalDescription(offer)
                     await b.setRemoteDescription(offer)
                     const answer = await b.createAnswer()
                     await b.setLocalDescription(answer)
                     await a.setRemoteDescription(answer)
                     await a.addIceCandidate({{ candidate: 'candidate:2 1 udp 2113937151 127.0.0.1 {port} typ host', sdpMid: '0', sdpMLineIndex: 0 }})
                     said.textContent += ' named'
                   }} catch (e) {{ said.textContent += ' failed: ' + e }}
                 }})()
               </script>"#
        ))])
        .await;
        let Some((mut tab, mut done)) = open(&pages, "/").await else { return };
        // Until the page has named its peer, and two seconds after that
        // for a check to arrive in.
        let mut datagram = [0u8; 1500];
        let mut arrived = None;
        let mut since_named = 0;
        for _ in 0..150 {
            arrived = tokio::time::timeout(Duration::from_millis(100), listening.recv_from(&mut datagram)).await.ok();
            if arrived.is_some() || since_named == 20 {
                break;
            }
            if done.seen.text.contains("pairing named") || done.seen.text.contains("failed") {
                since_named += 1;
            } else {
                done = tab.act(&Action::Read { more: false }).await.unwrap();
            }
        }
        assert!(arrived.is_none(), "a check reached 127.0.0.1:{port}, past the proxy: {arrived:?}");
        // Silence only counts from a page that did name its peer.
        assert!(done.seen.text.contains("pairing named"), "{}", done.seen.text);
        tab.close().await;
    }

    #[tokio::test]
    async fn nothing_is_fetched_ahead_of_a_press() {
        let _one_of_few = testing::gate().await;
        // A page may tell the browser which addresses to fetch, or to make
        // ready whole, before anyone goes there. Those requests are not the
        // main frame's, so nothing that judges the main frame's sees them.
        for kind in ["prerender", "prefetch"] {
            let pages = Pages::serve(&[
                home(&format!(
                    r#"<a href="/next">the next page</a> <a href="/checkout">Afrekenen</a>
                       <script type="speculationrules">{{"{kind}": [{{"source": "list", "urls": ["/next", "/checkout"]}}]}}</script>"#
                )),
                page("/next", "Next"),
                page("/checkout", "Checkout"),
            ])
            .await;
            let Some((mut tab, _)) = open(&pages, "/").await else { return };
            tokio::time::sleep(Duration::from_secs(2)).await;
            let read = tab.act(&Action::Read { more: false }).await.unwrap();
            assert_eq!((read.refused, pages.asked()), (NOTHING, vec!["GET /".to_string()]), "{kind}");
            // What is pressed is fetched then, and not before.
            assert_eq!(press(&mut tab, 1).await.seen.title, "Next", "{kind}");
            assert_eq!(pages.asked(), ["GET /", "GET /next"], "{kind}");
            tab.close().await;
        }
    }

    // ------------------------------------------------------------ typing

    /// A page of fields that writes down, where it can be read, the name
    /// of every field anything is typed into.
    const FIELDS: &str = r#"
        <form action="/search"><input type="search" name="q" placeholder="Zoeken"></form>
        <label>Aantal <input type="number" name="quantity" value="1"></label>
        <label>Postcode <input name="postcode" autocomplete="postal-code"></label>
        <label>Wachtwoord <input type="password" name="pw"></label>
        <label>Kaartnummer <input name="cardnumber" autocomplete="cc-number"></label>
        <label>Naam <input name="fullname" autocomplete="name"></label>
        <label>E-mailadres <input type="email" name="email"></label>
        <label>Telefoon <input type="tel" name="phone"></label>
        <label>Straat en huisnummer <input name="street" autocomplete="street-address"></label>
        <textarea name="message" aria-label="Opmerking"></textarea>
        <button type="button">Bewaar</button>
        <p>Typed into: <span id="typed">nothing</span></p>
        <script>
          document.addEventListener('input', (e) => {
            const typed = document.getElementById('typed')
            typed.textContent = (typed.textContent === 'nothing' ? '' : typed.textContent + ' ') + e.target.name
          })
        </script>"#;

    async fn type_into(tab: &mut Tab, n: u32, text: &str) -> Done {
        tab.act(&Action::Type { n, text: text.to_string(), enter: false }).await.expect("the browser answers")
    }

    #[tokio::test]
    async fn a_search_term_a_quantity_and_a_postcode_are_typed() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(FIELDS), page("/search", "Results")]).await;
        let Some((mut tab, first)) = open(&pages, "/").await else { return };
        assert!(first.seen.text.contains(r#"[1] field "Zoeken""#) && first.seen.text.contains(r#"[3] field "Postcode""#), "{}", first.seen.text);
        assert!(first.seen.text.contains("Typed into: nothing"), "{}", first.seen.text);

        let done = type_into(&mut tab, 1, "kattenvoer").await;
        assert_eq!(done.refused, NOTHING);
        assert!(done.seen.text.contains(r#"[1] field "Zoeken" = "kattenvoer""#), "{}", done.seen.text);
        // What is typed takes the place of what was there: two, not twelve.
        let done = type_into(&mut tab, 2, "2").await;
        assert!(done.seen.text.contains(r#"[2] field "Aantal" = "2""#), "{}", done.seen.text);
        let done = type_into(&mut tab, 3, "1011 PN").await;
        assert!(done.seen.text.contains(r#"[3] field "Postcode" = "1011 PN""#), "{}", done.seen.text);
        assert!(done.seen.text.contains("Typed into: q quantity postcode"), "{}", done.seen.text);

        // With Enter after it, a search term is sent.
        let done = tab.act(&Action::Type { n: 1, text: "kattenbak".to_string(), enter: true }).await.unwrap();
        assert_eq!((done.refused, done.seen.title.as_str()), (NOTHING, "Results"));
        assert!(pages.asked().contains(&"GET /search?q=kattenbak".to_string()), "{:?}", pages.asked());
        tab.close().await;
    }

    #[tokio::test]
    async fn nothing_personal_is_typed_whatever_is_asked() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(FIELDS)]).await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        // A password, a card, a name, an email, a phone, a street, a message.
        for n in 4..=10 {
            let done = type_into(&mut tab, n, "x").await;
            assert_eq!(done.refused.len(), 1, "field {n}: {:?}", done.refused);
            // Refused before a key is pressed: the page itself saw nothing.
            assert!(done.seen.text.contains("Typed into: nothing"), "field {n}: {}", done.seen.text);
        }
        assert_eq!(type_into(&mut tab, 4, "x").await.refused, ["a password field is never typed into"]);
        assert_eq!(type_into(&mut tab, 7, "x").await.refused, ["an email field is never typed into"]);
        assert_eq!(type_into(&mut tab, 5, "x").await.refused, ["that field is for something personal"]);
        // And a thing that is no field at all.
        assert_eq!(type_into(&mut tab, 11, "x").await.refused, ["that is not a field that takes a line of text"]);
        // Nor is the search box a way round it: what is typed is read too.
        let done = type_into(&mut tab, 1, "x@y.example").await;
        assert_eq!(done.refused, ["that is an email address, and a session types none"]);
        let done = type_into(&mut tab, 1, "4111 1111 1111 1111").await;
        assert_eq!(done.refused, ["that is the number of a payment card, and a session types none"]);
        assert!(done.seen.text.contains("Typed into: nothing"), "{}", done.seen.text);
        tab.close().await;
    }

    #[tokio::test]
    async fn more_than_64_characters_are_typed_nowhere() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(FIELDS)]).await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        let done = type_into(&mut tab, 3, &"1".repeat(65)).await;
        assert_eq!(done.refused, ["at most 64 characters may be typed"]);
        assert!(done.seen.text.contains("Typed into: nothing"), "{}", done.seen.text);
        let done = type_into(&mut tab, 3, &"1".repeat(64)).await;
        assert_eq!(done.refused, NOTHING);
        assert!(done.seen.text.contains("Typed into: postcode"), "{}", done.seen.text);
        tab.close().await;
    }

    /// A postcode field that does `on_focus` when it is given the caret,
    /// beside a password field, on a page that writes down the name and
    /// the kind of every field anything is typed into.
    fn a_field_that_answers_the_caret(on_focus: &str) -> (&'static str, testing::Reply) {
        home(&format!(
            r#"<label>Postcode <input name="postcode" onfocus="{on_focus}"></label>
               <label>Wachtwoord <input type="password" id="pw" name="pw"></label>
               <p>Typed into: <span id="typed">nothing</span></p>
               <script>
                 document.addEventListener('input', (e) => {{
                   const typed = document.getElementById('typed')
                   typed.textContent = (typed.textContent === 'nothing' ? '' : typed.textContent + ' ') + e.target.name + '/' + e.target.type
                 }})
               </script>"#
        ))
    }

    #[tokio::test]
    async fn a_field_that_hands_the_caret_on_as_it_is_given_it_is_not_typed_into() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[a_field_that_answers_the_caret("document.getElementById('pw').focus()")]).await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        let done = type_into(&mut tab, 1, "1011 PN").await;
        assert_eq!(done.refused, ["the page moved the caret away from that field, so nothing was typed"]);
        assert!(done.seen.text.contains("Typed into: nothing"), "{}", done.seen.text);
        tab.close().await;
    }

    #[tokio::test]
    async fn a_field_that_hands_the_caret_on_a_moment_later_has_been_typed_into_by_then() {
        let _one_of_few = testing::gate().await;
        // The judging, the caret and the typing were three calls, and a
        // page's script ran between them: handed on in a promise, the
        // caret was in the password field by the time anything was typed.
        for later in ["Promise.resolve().then(() => document.getElementById('pw').focus())", "setTimeout(() => document.getElementById('pw').focus(), 0)"] {
            let pages = Pages::serve(&[a_field_that_answers_the_caret(later)]).await;
            let Some((mut tab, _)) = open(&pages, "/").await else { return };
            let done = type_into(&mut tab, 1, "1011 PN").await;
            assert_eq!(done.refused, NOTHING, "{later}");
            // Typed where it was judged, and nowhere else.
            assert!(done.seen.text.contains("Typed into: postcode/text\n") || done.seen.text.ends_with("Typed into: postcode/text"), "{later}: {}", done.seen.text);
            assert!(done.seen.text.contains(r#"field "Postcode" = "1011 PN""#), "{later}: {}", done.seen.text);
            tab.close().await;
        }
    }

    #[tokio::test]
    async fn a_field_that_becomes_another_as_it_is_given_the_caret_is_not_typed_into() {
        let _one_of_few = testing::gate().await;
        // Judged as a postcode field, it was a password field by the time it was typed into.
        let pages = Pages::serve(&[a_field_that_answers_the_caret("this.type = 'password'; this.name = 'secret'")]).await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        let done = type_into(&mut tab, 1, "1011 PN").await;
        // Judged again as what it has become, and refused as that.
        assert_eq!(done.refused, ["a password field is never typed into"]);
        assert!(done.seen.text.contains("Typed into: nothing"), "{}", done.seen.text);
        tab.close().await;
    }

    #[tokio::test]
    async fn a_field_that_only_drops_its_grey_text_when_given_the_caret_is_typed_into() {
        let _one_of_few = testing::gate().await;
        // Changing on being given the caret is not in itself a trick: what
        // the field has become is judged, and this is still a postcode field.
        let pages = Pages::serve(&[a_field_that_answers_the_caret("this.placeholder = ''; this.title = 'Postcode'")]).await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        let done = type_into(&mut tab, 1, "1011 PN").await;
        assert_eq!(done.refused, NOTHING);
        assert!(done.seen.text.ends_with("Typed into: postcode/text"), "{}", done.seen.text);
        tab.close().await;
    }

    #[tokio::test]
    async fn a_field_that_is_another_every_time_it_is_looked_at_is_not_typed_into() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[a_field_that_answers_the_caret("this.title = 'Postcode ' + Math.random(); this.blur()")]).await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        let done = type_into(&mut tab, 1, "1011 PN").await;
        assert_eq!(done.refused.len(), 1, "{:?}", done.refused);
        assert!(done.seen.text.contains("Typed into: nothing"), "{}", done.seen.text);
        tab.close().await;
    }

    #[tokio::test]
    async fn a_page_that_answers_the_typing_itself_does_not_get_it_into_another_field() {
        let _one_of_few = testing::gate().await;
        // Not the caret being given, which is looked at again before
        // anything is typed, but the typing: a page that asks to hear when
        // text is about to go in, or has just gone in, and moves the caret
        // to the password field then.
        for hears in ["onbeforeinput", "oninput", "onkeydown", "onselect"] {
            let pages = Pages::serve(&[home(&format!(
                r#"<label>Postcode <input name="postcode" {hears}="document.getElementById('pw').focus()"></label>
                   <label>Wachtwoord <input type="password" id="pw" name="pw"></label>
                   <p>Typed into: <span id="typed">nothing</span></p>
                   <script>
                     document.addEventListener('input', (e) => {{
                       const typed = document.getElementById('typed')
                       typed.textContent = (typed.textContent === 'nothing' ? '' : typed.textContent + ' ') + e.target.name + '/' + e.target.type
                     }})
                   </script>"#
            ))])
            .await;
            let Some((mut tab, _)) = open(&pages, "/").await else { return };
            let done = type_into(&mut tab, 1, "1011 PN").await;
            // On Chrome 157 none of these gets the text anywhere but where
            // it was judged. Were one to, the session would end instead.
            assert_eq!(done.refused, NOTHING, "{hears}");
            assert!(done.seen.text.contains(r#"field "Postcode" = "1011 PN""#), "{hears}: {}", done.seen.text);
            assert!(done.seen.text.ends_with("Typed into: postcode/text"), "{hears}: {}", done.seen.text);
            tab.close().await;
        }
    }

    #[tokio::test]
    async fn typing_says_what_was_typed_into() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(FIELDS)]).await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        assert_eq!(type_into(&mut tab, 3, "1011 PN").await.notes, [r#"Typed into: field "Postcode""#]);
        tab.close().await;
    }

    #[tokio::test]
    async fn a_page_that_has_drifted_to_a_checkout_is_left_before_anything_is_done_there() {
        let _one_of_few = testing::gate().await;
        // The incident the reading recorded: a postcode typed on a checkout
        // page. This page becomes one by itself, with no request, a moment
        // after it is opened, and tells its server where it was typed into.
        let drifting = home(
            r#"<label>Postcode <input name="postcode"></label>
               <script>
                 addEventListener('input', () => navigator.sendBeacon('/typed-at' + location.pathname))
                 if (!sessionStorage.drifted) setTimeout(() => { sessionStorage.drifted = 1; history.pushState({}, '', '/checkout'); document.title = 'Checkout' }, 600)
               </script>"#,
        );
        let typed_at = |pages: &Pages| pages.asked().iter().filter(|line| line.contains("/typed-at")).cloned().collect::<Vec<_>>();

        let pages = Pages::serve(std::slice::from_ref(&drifting)).await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let done = type_into(&mut tab, 1, "1011 PN").await;
        assert_eq!(done.refused[0], format!("{}; the session went back to the page it was on", Refused::Checkout.why()));
        assert_eq!((done.seen.title.as_str(), done.seen.url.as_str()), ("Home", pages.url("/").as_str()));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(typed_at(&pages), NOTHING, "nothing was typed anywhere, least of all at the checkout");
        tab.close().await;

        // What that check is for: the same page and the same typing with
        // only the look at where the page is left out. This is the inside
        // of `act`, which no caller can reach.
        let pages = Pages::serve(&[drifting]).await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let mut refused = Vec::new();
        tab.type_into(1, "1011 PN", false, &mut refused, &mut Vec::new()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(typed_at(&pages), ["POST /typed-at/checkout"]);
        tab.close().await;
    }

    #[tokio::test]
    async fn a_link_that_may_not_be_followed_is_not_so_much_as_pressed() {
        let _one_of_few = testing::gate().await;
        // Refused where it is asked for would be too late for a page that
        // hears the press itself: this one writes down that it was pressed.
        let heard = "document.querySelector('h1').textContent = 'Pressed: ' + this.textContent";
        let pages = Pages::serve(&[
            home(&format!(
                r#"<a href="https://other.example/x" onmousedown="{heard}">another shop</a> <a href="/checkout" onmousedown="{heard}">Afrekenen</a>
                   <a href="/next#top" onmousedown="{heard}; return false" onclick="return false">the next page</a>"#
            )),
            page("/next", "Next"),
        ])
        .await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        for (n, why) in [(1, Refused::LeavesShop), (2, Refused::Checkout)] {
            let done = press(&mut tab, n).await;
            assert_eq!(done.refused, [why.why()]);
            assert!(done.seen.text.contains("\nHome\n"), "{}", done.seen.text);
        }
        // A link that may be followed is pressed, and the page hears it: so the two above were not.
        let done = press(&mut tab, 3).await;
        assert!(done.seen.text.contains("Pressed: the next page"), "{}", done.seen.text);
        tab.close().await;
    }

    // ------------------------------------- a list, the way back, another page

    async fn act(tab: &mut Tab, action: Action) -> Done {
        tab.act(&action).await.expect("the browser answers")
    }

    fn choose(n: u32, option: &str) -> Action {
        Action::Select { n, option: option.to_string() }
    }

    #[tokio::test]
    async fn an_option_is_chosen_from_a_list_as_the_page_words_it_or_nearly() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(
            r#"<select aria-label="Maat" onchange="document.getElementById('price').textContent = { M: '12,99', L: '13,99', XL: '14,99' }[this.value]">
                 <option value="M" selected>M</option><option value="L">L – 40x80 cm</option><option value="XL">XL (grote maat)</option>
               </select>
               <p>Prijs <span id="price">12,99</span></p>
               <a href="/next">the next page</a>"#,
        )])
        .await;
        let Some((mut tab, first)) = open(&pages, "/").await else { return };
        assert!(first.seen.text.contains(r#"[1] list "Maat": *M | L – 40x80 cm | XL (grote maat)"#), "{}", first.seen.text);

        let done = act(&mut tab, choose(1, "XL (grote maat)")).await;
        assert_eq!((done.refused, done.notes), (NOTHING, vec!["Chosen: xl (grote maat)".to_string()]));
        // The page heard of it: the price beside the list is the option's.
        assert!(done.seen.text.contains("Prijs 14,99") && done.seen.text.contains("*XL (grote maat)"), "{}", done.seen.text);
        // A hyphen for the page's dash, and no care for case or spaces.
        let done = act(&mut tab, choose(1, "l - 40X80 CM")).await;
        assert!(done.seen.text.contains("Prijs 13,99"), "{}", done.seen.text);
        // A part of the wording is enough where it is more than a letter.
        let done = act(&mut tab, choose(1, "grote maat")).await;
        assert!(done.seen.text.contains("Prijs 14,99"), "{}", done.seen.text);

        let done = act(&mut tab, choose(1, "XXL")).await;
        assert_eq!(done.refused, ["that list has no such option; it has: m | l – 40x80 cm | xl (grote maat)"]);
        assert!(done.seen.text.contains("Prijs 14,99"), "nothing was changed: {}", done.seen.text);
        let done = act(&mut tab, choose(2, "M")).await;
        assert_eq!(done.refused, ["that is not a list; press the option itself"]);
        let done = act(&mut tab, choose(9, "M")).await;
        assert_eq!(done.refused, ["nothing is numbered 9 on this page now; read it again"]);
        tab.close().await;
    }

    #[tokio::test]
    async fn a_list_that_sends_the_page_off_the_shop_is_held_like_a_link() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(
            r#"<select aria-label="Land" onchange="location.href = this.value">
                 <option value="">Kies een land</option><option value="https://other.example/de">Duitsland</option>
               </select>"#,
        )])
        .await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        let done = act(&mut tab, choose(1, "Duitsland")).await;
        assert_eq!(done.refused, [Refused::LeavesShop.why()]);
        assert_eq!((done.seen.title.as_str(), done.seen.url.as_str()), ("Home", pages.url("/").as_str()));
        tab.close().await;
    }

    #[tokio::test]
    async fn back_goes_to_the_page_before_and_never_to_before_the_shop() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(r#"<a href="/next">the next page</a>"#), page("/next", "Next")]).await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        // What stands before the page a session opened on is the empty tab Chrome began with.
        let done = act(&mut tab, Action::Back).await;
        assert_eq!(done.refused, ["there is no page of this shop to go back to"]);
        assert_eq!(done.seen.title, "Home");

        assert_eq!(press(&mut tab, 1).await.seen.title, "Next");
        let done = act(&mut tab, Action::Back).await;
        assert_eq!((done.refused, done.seen.title.as_str(), done.seen.url.as_str()), (NOTHING, "Home", pages.url("/").as_str()));
        tab.close().await;
    }

    #[tokio::test]
    async fn back_does_not_lead_to_an_address_a_session_was_taken_away_from() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(
            r#"<button onclick="history.pushState({}, '', '/checkout'); document.title = 'Checkout'">by a written address</button>"#,
        )])
        .await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        // The press writes a checkout's address, and the session is taken
        // back; the browser's own record still has the checkout in it.
        assert_eq!(press(&mut tab, 1).await.refused.len(), 1);
        let done = act(&mut tab, Action::Back).await;
        assert_eq!(done.refused, [Refused::Checkout.why()]);
        assert_eq!((done.seen.title.as_str(), done.seen.url.as_str()), ("Home", pages.url("/").as_str()));
        tab.close().await;
    }

    fn go_to(url: &str) -> Action {
        Action::Open { url: url.to_string() }
    }

    #[tokio::test]
    async fn another_page_of_the_shop_is_opened_by_its_address() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(""), page("/delivery", "Delivery"), ("/gone", status(404, "<title>Not here</title>"))]).await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        let done = act(&mut tab, go_to(&pages.url("/delivery"))).await;
        assert_eq!((done.refused, done.seen.title.as_str(), done.seen.status), (NOTHING, "Delivery", Some(200)));
        // A page that is not there is a page, with its status, and no refusal.
        let done = act(&mut tab, go_to(&pages.url("/gone"))).await;
        assert_eq!((done.refused, done.seen.title.as_str(), done.seen.status), (NOTHING, "Not here", Some(404)));
        tab.close().await;
    }

    #[tokio::test]
    async fn an_address_a_session_may_not_be_at_is_not_opened() {
        let _one_of_few = testing::gate().await;
        let neighbour = Pages::serve(&[("/secret", html("<title>Secret</title>secret_total 1"))]).await;
        let pages = Pages::serve(&[
            home(""),
            page("/checkout", "Checkout"),
            page("/account/login", "Sign in"),
            ("/out", redirect("https://other.example/landed")),
        ])
        .await;
        let Some((mut tab, _)) = open(&pages, "/").await else { return };
        for (address, why) in [
            ("https://other.example/p/1".to_string(), Refused::LeavesShop.why()),
            (pages.url("/checkout"), Refused::Checkout.why()),
            (pages.url("/account/login"), Refused::SignIn.why()),
            ("file:///etc/passwd".to_string(), Refused::NotWeb.why()),
            ("not an address".to_string(), Refused::NotWeb.why()),
            // The shop's own page, with more after the question mark than an address is for.
            (format!("{}?q={}", pages.url("/"), "x".repeat(ADDRESS_CHARS)), Refused::TooLong.why()),
            // The shop's own page that sends on to another shop.
            (pages.url("/out"), Refused::LeavesShop.why()),
            // The same host by number on a port the guard does not let through.
            (neighbour.url("/secret"), NOT_PUBLIC),
        ] {
            let done = act(&mut tab, go_to(&address)).await;
            assert_eq!(done.refused, [why], "{address}");
            assert_eq!((done.seen.title.as_str(), done.seen.url.as_str()), ("Home", pages.url("/").as_str()), "{address}");
        }
        assert_eq!(pages.asked(), ["GET /", "GET /out"]);
        assert_eq!(neighbour.asked(), NOTHING);
        tab.close().await;
    }

    #[tokio::test]
    async fn the_page_gone_back_to_is_shown_with_its_own_status_not_the_last_ones() {
        let _one_of_few = testing::gate().await;
        // Going back brings a page out of the browser's keeping with no
        // request and no answer, so the last status heard is another page's.
        let pages = Pages::serve(&[home(r#"<a href="/blocked">a page the shop refuses</a>"#), ("/blocked", status(403, "<title>Shop</title>Nee.")), page("/next", "Next")]).await;
        let Some((mut tab, first)) = open(&pages, "/").await else { return };
        assert_eq!(first.seen.status, Some(200));
        assert_eq!(press(&mut tab, 1).await.seen.status, Some(403));
        let back = act(&mut tab, Action::Back).await;
        assert_eq!((back.seen.title.as_str(), back.seen.status), ("Home", Some(200)));
        // And a page that is opened and then refused on its way elsewhere keeps its own.
        let stayed = act(&mut tab, go_to("https://other.example/")).await;
        assert_eq!((stayed.seen.title.as_str(), stayed.seen.status), ("Home", Some(200)));
        tab.close().await;
    }

    #[tokio::test]
    async fn a_page_is_shown_with_the_status_it_came_with() {
        let _one_of_few = testing::gate().await;
        let pages = Pages::serve(&[home(r#"<a href="/blocked">a page the shop refuses</a>"#), ("/blocked", status(403, "<title>Shop</title>Nee."))]).await;
        let Some((mut tab, first)) = open(&pages, "/").await else { return };
        assert_eq!(first.seen.status, Some(200));
        let done = press(&mut tab, 1).await;
        assert_eq!((done.seen.status, done.seen.title.as_str()), (Some(403), "Shop"));
        tab.close().await;
    }
}
