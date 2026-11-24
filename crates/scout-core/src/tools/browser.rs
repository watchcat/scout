//! Headless-Chrome fallback for pages plain HTTP cannot read.
//!
//! Used only when a normal fetch fails, because it costs seconds and a few
//! hundred MB per page. It earns that on shops behind a challenge: action.com
//! answers 403 to our HTTP client, while a real browser resolves the
//! challenge and yields the product page — 3.98 EUR, in stock, from the same
//! extractors that read every other shop.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;

/// Chrome fast-forwards page timers up to this budget, so a challenge that
/// waits several seconds of wall time clears in a fraction of it.
const VIRTUAL_TIME_BUDGET_MS: u32 = 30_000;
/// Once the DOM starts arriving it comes in one burst; a lull this long
/// means Chrome has finished and is merely refusing to exit.
const QUIET_AFTER_OUTPUT: Duration = Duration::from_secs(3);
/// Everything about one render is bounded by this — the wait for a free slot
/// included. A page worth more than this is not worth blocking a chat reply
/// for.
pub const RENDER_TIMEOUT: Duration = Duration::from_secs(45);

/// Renders allowed at once across the whole process.
///
/// Each one is a separate Chrome, and the module doc's "a few hundred MB per
/// page" is per process: ten people asking for a walled page at the same
/// moment is several gigabytes, on a box that has already been killed by the
/// out-of-memory reaper for less. Two keeps a single slow render from
/// blocking every other request while staying inside what the container has.
const MAX_CONCURRENT_RENDERS: usize = 2;

#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    #[error("headless browser failed: {0}")]
    Launch(String),
    #[error("rendering timed out")]
    Timeout,
    #[error("too many pages are already rendering")]
    Busy,
    /// Not an address a render is for, said before any Chrome was started:
    /// the sentence is the reason, for whoever asked.
    #[error("{0}")]
    Refused(String),
}

/// Marks of an interstitial rather than the page asked for.
///
/// The title is the reliable signal. Body markers are not: a page that has
/// *passed* Cloudflare still carries `challenge-platform` script tags, and
/// treating those as a challenge means never recognising success — which is
/// exactly how the first version of this burned its whole budget on a page
/// it had already loaded.
fn is_challenge(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    let title = lower
        .split_once("<title")
        .and_then(|(_, rest)| rest.split_once('>'))
        .and_then(|(_, rest)| rest.split_once("</title>"))
        .map(|(t, _)| t.trim().to_string())
        .unwrap_or_default();
    [
        "just a moment",
        "checking your browser",
        "attention required",
        "access denied",
        // Reddit's wall answers HTTP 200 with a plausible-looking shell, so
        // without this it reads as a page that simply had nothing on it.
        "please wait for verification",
        "verifying you are human",
    ]
    .iter()
    .any(|m| title.contains(m))
        || lower.contains("cf-chl-widget")
}

/// A body that plain fetching cannot use: an interstitial, or a shell so
/// small its content clearly arrives by script.
pub fn looks_unrendered(html: &str) -> bool {
    is_challenge(html) || html.len() < 2048
}

/// Whether a body is a bot check. Exposed so `fetch_page` can fail loudly
/// rather than hand an interstitial back as if it were the page.
pub fn is_challenge_page(html: &str) -> bool {
    is_challenge(html)
}

/// Where Chrome lives, if it does. Resolved once at startup: without it the
/// fallback stays off and plain fetching behaves exactly as before.
pub fn find_chrome() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("SCOUT_CHROME") {
        let path = PathBuf::from(path);
        return path.exists().then_some(path);
    }
    [
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/google-chrome",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
}

/// The most of a page the browser pod's answer is read for.
///
/// The pod is where a page that took a Chrome over would be, so what the
/// pod answers is that page's to make, its length included; uncapped, it
/// could be as long as Scout has memory. The longest render measured was
/// 598 KB (`docs/BOARD.md`, "The render's Chrome, measured where it runs").
pub const MAX_REMOTE_PAGE: usize = 16 * 1024 * 1024;

/// Renders `url` and returns the DOM after scripts have run.
///
/// Cloned into every `fetch_page` tool, so the slots are shared: the limit is
/// on renders under way in this process, not on renders per request. For a
/// local renderer that is Chromes alive here; for one that asks the pod it
/// is backpressure, and the pod's own slots are the cap.
#[derive(Clone)]
pub struct Renderer {
    how: How,
    slots: Arc<Semaphore>,
    /// What the proxy Chrome goes through will open: the public internet,
    /// except in a live test, whose page is on loopback.
    guard: crate::net_guard::Guard,
    /// Where each render's profile is made and removed again: the temp
    /// directory, except in a test that looks at what a render leaves there.
    profiles: PathBuf,
    /// What a test's renderer answers every address with, in place of a Chrome.
    #[cfg(test)]
    canned: Option<Arc<str>>,
    /// What a live test does to a render before its Chrome is started, to
    /// the profile at the path it is given and to the command line: one of
    /// two settings taken out to see the other hold alone, a proxy of the
    /// test's own in place of the guard's.
    #[cfg(test)]
    before_chrome: Option<BeforeChrome>,
}

#[cfg(test)]
type BeforeChrome = Arc<dyn Fn(&std::path::Path, &mut Vec<String>) + Send + Sync>;

/// Where a render happens.
#[derive(Clone)]
enum How {
    /// In a Chrome of this process's own, started for the render.
    Local(PathBuf),
    /// In the browser pod, asked over HTTP: production's, so that a page
    /// that takes its Chrome over is in a pod with nothing of Scout's.
    Remote(Remote),
}

/// The browser pod, as Scout asks it.
#[derive(Clone)]
struct Remote {
    /// `POST` here, `{"url": …}`; the page comes back as the body.
    render: url::Url,
    client: reqwest::Client,
    /// [`MAX_REMOTE_PAGE`], except in a test of the cap.
    cap: usize,
}

impl Renderer {
    pub fn new(chrome: PathBuf) -> Self {
        Self {
            how: How::Local(chrome),
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT_RENDERS)),
            guard: crate::net_guard::Guard::PUBLIC_ONLY,
            profiles: std::env::temp_dir(),
            #[cfg(test)]
            canned: None,
            #[cfg(test)]
            before_chrome: None,
        }
    }

    /// A renderer that asks the browser pod at `pod`, an address like
    /// `http://scout-browser:8091`, for every render.
    ///
    /// What the pod answers is read as a page fetched from anywhere is:
    /// untrusted. Its length is capped, and its client follows no redirect
    /// and goes through no proxy from the environment. Unlike the open
    /// web's client it has no guard, because the pod is on the cluster's
    /// private addresses; a redirect it followed would take Scout's own
    /// requests to wherever a taken-over pod pointed them.
    pub fn remote(pod: &url::Url) -> Result<Self, BrowserError> {
        let render = pod.join("render").map_err(|e| BrowserError::Launch(format!("the browser pod's address: {e}")))?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(RENDER_TIMEOUT)
            .build()
            .map_err(|e| BrowserError::Launch(e.to_string()))?;
        let local = Self::new(PathBuf::new());
        Ok(Self { how: How::Remote(Remote { render, client, cap: MAX_REMOTE_PAGE }), ..local })
    }

    /// Which form this is, for the log line at start.
    pub fn describe(&self) -> String {
        match &self.how {
            How::Local(chrome) => format!("a Chrome of this process's own, {}", chrome.display()),
            How::Remote(remote) => format!("the browser pod, {}", remote.render),
        }
    }

    /// A renderer whose Chrome reaches `mock` and nothing else, so that a
    /// live test can show a real Chrome a page of its own making.
    #[cfg(test)]
    fn reaching_only(chrome: PathBuf, mock: std::net::SocketAddr) -> Self {
        Self { guard: crate::net_guard::Guard::only(mock), ..Self::new(chrome) }
    }

    /// A renderer that hands back `html` whatever it is asked for, so that
    /// what `fetch_page` does with a render can be tested without a Chrome.
    #[cfg(test)]
    pub(crate) fn answering(html: &str) -> Self {
        Self { canned: Some(html.into()), ..Self::new(PathBuf::from("/no/chrome/needed")) }
    }

    pub async fn render(&self, url: &str) -> Result<String, BrowserError> {
        #[cfg(test)]
        if let Some(html) = &self.canned {
            return Ok(html.to_string());
        }
        let started = Instant::now();
        // Inside the deadline: deciding resolves the name, and a resolver
        // that does not answer is no reason for a render to outlast it.
        match tokio::time::timeout(RENDER_TIMEOUT, self.may_render(url)).await {
            Ok(decided) => decided?,
            Err(_) => return Err(BrowserError::Timeout),
        }
        // Queueing is part of the deadline rather than added to it, so a
        // busy renderer cannot turn a 45-second ceiling into ninety.
        let left = RENDER_TIMEOUT.saturating_sub(started.elapsed());
        let _slot = match tokio::time::timeout(left, self.slots.acquire()).await {
            Ok(Ok(slot)) => slot,
            Ok(Err(e)) => return Err(BrowserError::Launch(e.to_string())),
            Err(_) => return Err(BrowserError::Busy),
        };
        let left = RENDER_TIMEOUT.saturating_sub(started.elapsed());
        // Waited so long there is no point starting: `timeout` fires before
        // the first poll, so no Chrome is spawned only to be killed.
        tokio::time::timeout(left, self.render_inner(url))
            .await
            .map_err(|_| BrowserError::Timeout)?
    }

    /// Whether `url` is a page a render may be asked for: a web address,
    /// on the public internet as this renderer's guard sees it.
    ///
    /// `fetch_page` asks only for such pages, having checked them itself.
    /// This is for whoever else asks, and since the browser pod is asked
    /// over HTTP, that is anyone who can reach the pod. A Chrome told
    /// `--dump-dom` with a `file:` address prints the file. With an address
    /// on loopback it is the proxy that refuses, but only once Chrome is
    /// running, and Chrome then prints its own error page as the page.
    async fn may_render(&self, url: &str) -> Result<(), BrowserError> {
        let parsed = url::Url::parse(url).map_err(|_| BrowserError::Refused("that is not a web address".to_string()))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(BrowserError::Refused(format!(
                "only a web page is rendered, and that address is {}:",
                parsed.scheme()
            )));
        }
        self.guard
            .check(&parsed)
            .await
            .map_err(|_| BrowserError::Refused(crate::net_guard::NOT_PUBLIC.to_string()))
    }

    /// The render itself, once it has been decided on and has its slot.
    async fn render_inner(&self, url: &str) -> Result<String, BrowserError> {
        match &self.how {
            How::Local(chrome) => self.render_here(chrome, url).await,
            How::Remote(remote) => remote.render(url).await,
        }
    }

    /// One-shot `--dump-dom`, deliberately not a CDP session.
    ///
    /// Driving Chrome over CDP is the obvious approach and it does not work
    /// here: an automated session announces itself (`navigator.webdriver`
    /// among others) and action.com's challenge never clears — measured at
    /// 67 seconds of real time, still the interstitial. The same Chrome as a
    /// plain subprocess passes it every time.
    async fn render_here(&self, chrome: &std::path::Path, url: &str) -> Result<String, BrowserError> {
        // Each render gets a throwaway profile: concurrent runs sharing one
        // would fight over its lock. Its preferences are on disk before
        // Chrome is, and without them there is no render.
        let profile = Profile::create_in(&self.profiles)
            .map_err(|e| BrowserError::Launch(format!("could not write the profile: {e}")))?;

        // `fetch_page` checked the URL, but Chrome follows redirects, runs
        // the page's scripts and resolves names on its own — any of which
        // can land on the pod's own listeners. Every connection it makes
        // goes through this proxy instead, which applies the same fence.
        let (proxy, _proxy) = crate::net_guard::serve_socks(self.guard)
            .await
            .map_err(|e| BrowserError::Launch(e.to_string()))?;
        // Started after the profile is made, so let go of before it on any
        // way out of here: the profile is removed from under processes that
        // have been killed, not from under ones still at work.
        let args = chrome_args(url, &profile, proxy);
        #[cfg(test)]
        let args = self.as_its_test_would_have_it(&profile, args);
        let mut chrome = Chrome::start(chrome, args).map_err(|e| BrowserError::Launch(e.to_string()))?;

        // Chrome prints the DOM and then lingers — waiting for it to exit
        // means waiting forever (measured: the full page in stdout, the
        // process still alive two minutes later). So read the pipe instead,
        // and stop once it goes quiet.
        let mut stdout = chrome.0.stdout.take().expect("stdout is piped");
        let mut html = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            // Nothing arrives until the page has loaded, so the first read
            // gets the full deadline; afterwards a short lull means done.
            let patience = if html.is_empty() { RENDER_TIMEOUT } else { QUIET_AFTER_OUTPUT };
            match tokio::time::timeout(patience, stdout.read(&mut chunk)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => html.extend_from_slice(&chunk[..n]),
                Ok(Err(e)) => return Err(BrowserError::Launch(e.to_string())),
                Err(_) => break,
            }
        }
        chrome.kill();
        let _ = chrome.0.wait().await;
        drop(chrome);
        drop(profile);

        if html.is_empty() {
            return Err(BrowserError::Timeout);
        }
        Ok(String::from_utf8_lossy(&html).into_owned())
    }

    #[cfg(test)]
    fn as_its_test_would_have_it(&self, profile: &Profile, mut args: Vec<String>) -> Vec<String> {
        if let Some(change) = &self.before_chrome {
            change(profile.path(), &mut args);
        }
        args
    }
}

impl Remote {
    /// One render, asked of the pod. The pod answers as `scout-browser`'s
    /// `POST /render` does: the page as the body, or a status and
    /// `{"error": …}` that are turned back into the error it was there.
    async fn render(&self, url: &str) -> Result<String, BrowserError> {
        let mut answer = self
            .client
            .post(self.render.clone())
            .json(&serde_json::json!({ "url": url }))
            .send()
            .await
            .map_err(|e| if e.is_timeout() { BrowserError::Timeout } else { BrowserError::Launch(format!("the browser pod could not be asked: {e}")) })?;
        let status = answer.status();
        if status == reqwest::StatusCode::OK {
            return self.page(&mut answer).await;
        }
        // An error's sentence is short; a longer one is not read.
        let said = read_capped(&mut answer, 4096).await.ok().unwrap_or_default();
        let why = serde_json::from_slice::<serde_json::Value>(&said)
            .ok()
            .and_then(|body| body["error"].as_str().map(str::to_string));
        Err(match status.as_u16() {
            503 => BrowserError::Busy,
            504 => BrowserError::Timeout,
            422 => BrowserError::Refused(why.unwrap_or_else(|| "the browser pod refused that address".to_string())),
            _ => BrowserError::Launch(match why {
                Some(why) => format!("the browser pod answered {status}: {why}"),
                None => format!("the browser pod answered {status}"),
            }),
        })
    }

    /// The page, read up to the cap and no further.
    async fn page(&self, answer: &mut reqwest::Response) -> Result<String, BrowserError> {
        match read_capped(answer, self.cap).await {
            Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
            Err(Some(e)) if e.is_timeout() => Err(BrowserError::Timeout),
            Err(Some(e)) => Err(BrowserError::Launch(format!("the browser pod's page could not be read: {e}"))),
            Err(None) => Err(BrowserError::Launch(format!("the browser pod's page was longer than {} bytes", self.cap))),
        }
    }
}

/// A body read in pieces until it ends or passes `cap`: `Err(None)` when
/// it would have, without reading what was beyond.
async fn read_capped(answer: &mut reqwest::Response, cap: usize) -> Result<Vec<u8>, Option<reqwest::Error>> {
    if answer.content_length().is_some_and(|said| said > cap as u64) {
        return Err(None);
    }
    let mut body = Vec::new();
    while let Some(piece) = answer.chunk().await.map_err(Some)? {
        if body.len() + piece.len() > cap {
            return Err(None);
        }
        body.extend_from_slice(&piece);
    }
    Ok(body)
}

/// One render's Chrome and everything that Chrome started, stopped together.
///
/// Chrome is several processes: the one started here, and a renderer, a
/// network service, a GPU process and more that it starts itself. Killing
/// the first leaves the rest to notice and end themselves, and until they
/// do they write where they were writing, which is the profile. So the
/// Chrome is made the leader of a process group of its own, which its
/// helpers are born into, and it is the group that is signalled — when the
/// page has been read, and by the drop on every other way out: a Chrome
/// that would not print, a deadline that drops the render mid-read.
/// `scout-browser` stops its Chromes the same way (`pipe.rs`, which has the
/// count of helpers found in their Chrome's group).
///
/// On the Chrome measured the helpers do notice, and fast: 2026-11-22 on
/// Chrome 157, with the group's signal taken out, 32 renders cut short had
/// no process left a tenth of a second later and left no profile. The
/// signal is here so that nothing rests on what a killed program's children
/// do next, and the tests that hold it use a stand-in whose second process
/// does not notice.
///
/// Not everything a Chrome starts is in its group. Debian's Chromium 156
/// puts its two crash handlers in sessions of their own, which no signal to
/// the group reaches. They end themselves when their Chrome does (the same
/// day, in the image: gone within 0.3 s of the first process alone being
/// killed) and they keep their files in the user's home, not the profile.
///
/// One cost, by the rule and not by measurement: a terminal's Ctrl-C goes
/// to its foreground group, which a Chrome in a group of its own is not in.
/// A bot interrupted at a terminal in the middle of a render leaves that
/// render's Chrome and profile behind. In the container nothing is typed,
/// and what stops the bot stops the container.
struct Chrome(tokio::process::Child);

impl Chrome {
    fn start(program: &std::path::Path, args: Vec<String>) -> std::io::Result<Self> {
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            // Nothing of ours to read: in a group of its own a Chrome that
            // read a terminal's input would be stopped for it.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        command.spawn().map(Self)
    }

    /// Kills the Chrome and its helpers. Sent, not waited for: what a
    /// killed process had under way is the profile's to outlast.
    fn kill(&mut self) {
        // `id` is the Chrome's number, and its group's, for as long as the
        // Chrome has not been waited for, and for that long the system
        // gives the number to nothing else. Once it has been there is no
        // number here, so a second kill cannot signal a stranger's group.
        #[cfg(unix)]
        if let Some(group) = self.0.id().and_then(|id| i32::try_from(id).ok()).and_then(rustix::process::Pid::from_raw) {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
        let _ = self.0.start_kill();
    }
}

impl Drop for Chrome {
    fn drop(&mut self) {
        self.kill();
    }
}

/// What a render's profile says before Chrome first reads it: three
/// settings Chrome keeps in its profile, written there because that is where
/// it looks for them.
///
/// `webrtc.ip_handling_policy`. The proxy carries TCP and WebRTC speaks UDP,
/// from sockets Chrome opens itself. Measured 2026-11-20 on Chrome 157 with
/// the flags below and no such setting: a page naming a STUN server on
/// loopback had a datagram delivered there, by literal and by `localhost`
/// alike, and was handed two host candidates. Measured the day after: a
/// TURN server is reached the same way, and so is any address a page gives
/// one of its own connections as its peer's, with no server named at all.
/// `disable_non_proxied_udp` leaves WebRTC only what the proxy can carry,
/// and this proxy carries no UDP: with it nothing arrived by any of those
/// ways and gathering ended with no candidate at all, so a page's script
/// learns no address either. Of the switches, the one in `chrome_args` does
/// the same on its own and is passed as well;
/// `--force-webrtc-ip-handling-policy` was measured too and changes nothing.
///
/// `net.network_prediction_options`, 2 for never. A page's speculation rules
/// had Chrome prefetch one address and prerender another during a render
/// (same day, same Chrome). Those loads go through the proxy — the one aimed
/// at a second loopback listener was refused there — so this is not a way
/// out; it is a page spending the render on addresses nobody asked for.
/// With the setting neither request was made.
///
/// `browser.enable_spellchecking`, off. Nothing a render does is spelt, and
/// with it on Debian's Chromium asks its maker for a dictionary on every
/// start, since every start is a new profile's first: measured 2026-11-22
/// on Chromium 156, the image's, `redirector.gvt1.com` was asked for
/// `en-us-10-2.bdic` in 36 of 37 starts without the setting and in none of
/// 32 with it. `spellcheck.dictionaries` set to none was measured as well
/// and changes nothing. (Google's own Chrome on a Mac spells with the
/// system and asks for no dictionary either way.) It is one of the things a
/// Chrome asks for on its own account; `chrome_args` ends another, and the
/// `live` tests have what is left.
const PREFERENCES: &str = r#"{"webrtc":{"ip_handling_policy":"disable_non_proxied_udp"},"net":{"network_prediction_options":2},"browser":{"enable_spellchecking":false}}"#;

/// One render's throwaway profile: on disk, preferences written, from
/// [`Profile::create_in`] until it is dropped.
///
/// It used to be a path Chrome made on first use and the render removed on
/// its way out. Now that it exists before Chrome does, removing it belongs
/// to every way out — Chrome that would not start, a deadline that drops
/// the render mid-read — and a drop is the one place all of them pass.
///
/// One removal was not the end of it. Only the first of Chrome's processes
/// was killed, and on the deadline's way out, where nothing is waited for,
/// the directory went from under the others: measured 2026-11-21 on Chrome
/// 157, of 48 renders cut between 0.2 and 4 seconds in, two left their
/// directory behind, files and all. Now the whole Chrome is killed first
/// ([`Chrome`]), and the drop goes on removing the directory until it has
/// stayed away (`LOOK_AGAIN`), since a kill sent is not yet a process
/// dead. Measured 2026-11-22 by `renders_cut_short_leave_no_chrome_and_no_profile`,
/// renders cut the same way: 96 on Chrome 157, 48 on Debian's Chromium 156
/// in the image and 32 on the node, and none left a directory or a process.
///
/// That test holds the two together and neither alone: with either taken
/// out and the other left, a real Chrome's renders still left nothing (80
/// without the kill and 16 without the repeat, most of them the review's
/// runs), and only with both out did a profile stay. What holds each by
/// itself is a test with a stand-in:
/// `a_render_cut_short_takes_all_of_its_chrome_…` for the kill,
/// `a_profile_still_being_written_into_…` for the repeat.
struct Profile(PathBuf);

impl Profile {
    /// Fails rather than hand back a profile without its preferences: a
    /// Chrome started on a bare one sends UDP wherever a page points it.
    fn create_in(base: &std::path::Path) -> std::io::Result<Self> {
        // The count is what keeps two renders apart. The clock alone does
        // not: sixteen threads making sixty-four profiles each landed
        // twenty of them in a directory another already had (2026-11-21, on
        // a Mac, whose clock ticks in microseconds).
        static MADE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = base.join(format!(
            "scout-chrome-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default(),
            MADE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        // The temp directory itself may be there or not, as it could be when
        // Chrome made the whole path. The profile is another matter — made,
        // never adopted: a directory already there is not ours to write
        // into, nor — which is why this comes before the value that removes
        // on drop — ours to remove.
        std::fs::create_dir_all(base)?;
        private_dir(&path)?;
        let profile = Self(path);
        // `Default` is the profile Chrome uses inside a fresh data directory.
        let default = profile.0.join("Default");
        private_dir(&default)?;
        let mut preferences = std::fs::OpenOptions::new();
        preferences.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut preferences, 0o600);
        std::io::Write::write_all(&mut preferences.open(default.join("Preferences"))?, PREFERENCES.as_bytes())?;
        Ok(profile)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

/// Makes one directory, its owner's alone, as Chrome makes its own profile
/// (`a_profile_is_its_owners_alone` has the modes).
fn private_dir(path: &std::path::Path) -> std::io::Result<()> {
    let mut dir = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut dir, 0o700);
    dir.create(path)
}

/// How often a profile that has been let go of is removed and looked for
/// again, how many looks running must find it gone, and when one that
/// something keeps writing is given up on: a hundred and fifty milliseconds
/// of quiet, two seconds at most. `scout-browser`'s numbers for the same
/// wait, chosen there for a process's last moments and not measured.
const LOOK_AGAIN: Duration = Duration::from_millis(50);
const GONE_FOR: u32 = 3;
const GIVE_UP: Duration = Duration::from_secs(2);

impl Drop for Profile {
    fn drop(&mut self) {
        // Once here, so that a profile nothing is writing into is gone by
        // the time the render is; and then for as long as it takes, on a
        // thread of its own, since this runs on the runtime's in the middle
        // of whatever let go of the render.
        let _ = std::fs::remove_dir_all(&self.0);
        let path = std::mem::take(&mut self.0);
        let left = path.clone();
        let watching = std::thread::Builder::new().name("scout-chrome-profile".into()).spawn(move || {
            let (mut quiet, since) = (0, Instant::now());
            while quiet < GONE_FOR && since.elapsed() < GIVE_UP {
                std::thread::sleep(LOOK_AGAIN);
                // Found gone, not made gone: a directory just removed is
                // away for the moment whoever is writing it or not, and
                // counting that would end the wait under a writer's feet.
                quiet = if path.exists() { 0 } else { quiet + 1 };
                let _ = std::fs::remove_dir_all(&path);
            }
            if path.exists() {
                tracing::warn!(profile = %path.display(), "a render's profile is still being written into and was left behind");
            }
        });
        if let Err(e) = watching {
            tracing::warn!(profile = %left.display(), "a render's profile was removed once and not looked for again: {e}");
        }
    }
}

/// Chrome's command line for one render through the proxy on `proxy`.
///
/// It takes a [`Profile`] and not a path, so that a command line can only be
/// made for a profile whose preferences are already on disk.
fn chrome_args(url: &str, profile: &Profile, proxy: u16) -> Vec<String> {
    vec![
        "--headless=new".into(),
        "--dump-dom".into(),
        format!("--virtual-time-budget={VIRTUAL_TIME_BUDGET_MS}"),
        format!("--user-data-dir={}", profile.path().display()),
        format!("--user-agent={}", super::fetch::BROWSER_USER_AGENT),
        // socks5, not socks4 or http: Chrome then sends the hostname and
        // leaves resolving it to the proxy, so the name is checked and
        // dialled in one place and cannot be rebound in between.
        format!("--proxy-server=socks5://127.0.0.1:{proxy}"),
        // Chrome sends localhost and loopback around any proxy unless told
        // not to — which would walk straight past the fence to :9090.
        "--proxy-bypass-list=<-loopback>".into(),
        // The profile's preferences say this already (`PREFERENCES`), and
        // each way holds without the other: measured 2026-11-20 on Chrome
        // 157, with this switch and empty preferences a page's STUN datagram
        // reached none of three loopback listeners and no ICE candidate was
        // gathered, the same as with the preference and no switch; the day
        // after, the same of a TURN server and of a peer the page names.
        // Said twice because either is ignored in silence by a Chrome that
        // does not know it, and a render with neither sends UDP where a page
        // points it.
        "--webrtc-ip-handling-policy=disable_non_proxied_udp".into(),
        // A Chrome asks its maker what time it is at every start, over the
        // proxy, which opens it: measured 2026-11-22, `clients2.google.com`
        // in every start without this, 31 of Debian's Chromium 156 and 13
        // of Chrome 157, and in none with it, 39 and 17. Chrome keeps the last
        // `--disable-features` it is given, so anything else to be turned
        // off this way goes into this one, after a comma.
        "--disable-features=NetworkTimeServiceQuerying".into(),
        // --no-sandbox is required to run as a non-root user in a
        // container; --disable-dev-shm-usage keeps Chrome from dying on
        // the small /dev/shm a container gets.
        "--no-sandbox".into(),
        "--disable-dev-shm-usage".into(),
        "--disable-gpu".into(),
        "--disable-extensions".into(),
        "--no-first-run".into(),
        "--window-size=1280,1024".into(),
        "--lang=nl-NL".into(),
        "--mute-audio".into(),
        url.into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_goes_through_the_guard_proxy_loopback_included() {
        let base = tempfile::tempdir().unwrap();
        let profile = Profile::create_in(base.path()).unwrap();
        let args = chrome_args("https://shop.example/p", &profile, 4321);
        // A command line is made from a profile that has been written, so
        // that there is no Chrome without its preferences; and it is that
        // profile Chrome is sent to.
        assert!(args.contains(&format!("--user-data-dir={}", profile.path().display())));
        assert!(args.contains(&"--proxy-server=socks5://127.0.0.1:4321".to_string()));
        // Without this Chrome reaches 127.0.0.1 directly, proxy or not.
        assert!(args.contains(&"--proxy-bypass-list=<-loopback>".to_string()));
        assert_eq!(args.last().map(String::as_str), Some("https://shop.example/p"));
    }

    #[test]
    fn chrome_is_told_on_its_command_line_too_to_keep_webrtc_in_the_proxy() {
        // The second of two ways it is told; the profile's preferences are
        // the first. Not the `--force-…` spelling, which Chrome ignores.
        let base = tempfile::tempdir().unwrap();
        let profile = Profile::create_in(base.path()).unwrap();
        let args = chrome_args("https://shop.example/p", &profile, 4321);
        assert!(args.contains(&"--webrtc-ip-handling-policy=disable_non_proxied_udp".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("--force-webrtc-ip-handling-policy")));
    }

    #[test]
    fn challenge_pages_are_recognised_by_title_not_by_leftover_scripts() {
        // What action.com serves before the challenge clears.
        assert!(is_challenge("<html><head><title>Just a moment...</title></head></html>"));
        assert!(is_challenge(
            "<html><head><title>Checking your browser</title></head></html>"
        ));
        assert!(is_challenge(r#"<html><body><div id="cf-chl-widget"></div></body></html>"#));

        // The page AFTER passing: real title, and Cloudflare's own scripts
        // still in the DOM. Reading those as a challenge is what made the
        // first version spin until its deadline.
        let passed = "<html><head><title>Vanish Oxi Action vlekverwijderaar Kleur | Action NL\
                      </title></head><body><script src=\"/cdn-cgi/challenge-platform/x.js\">\
                      </script></body></html>";
        assert!(!is_challenge(passed));
        assert!(!looks_unrendered(&format!("{passed}{}", "padding ".repeat(400))));
    }

    #[test]
    fn reddits_wall_is_a_challenge_despite_answering_200() {
        // The exact title www.reddit.com serves to a plain GET (measured
        // 2026-08-05: HTTP 200, 8.5 KB, no thread content). It used to fall
        // through as ordinary page text, so the model read it as a page with
        // nothing on it and opened three more just like it.
        let wall = "<html><head><title>Reddit - Please wait for verification</title>\
                    </head><body></body></html>";
        assert!(is_challenge(wall));
        assert!(is_challenge_page(wall));

        // A real thread title from the same subreddit must not match.
        let real = "<html><head><title>Who’s got the CM-15? : teenageengineering\
                    </title></head><body>comments</body></html>";
        assert!(!is_challenge(real));
    }

    #[test]
    fn chrome_is_found_through_the_env_override_when_set() {
        // A path that does not exist must not enable the fallback.
        temp_env_var("SCOUT_CHROME", "/definitely/not/here", || {
            assert!(find_chrome().is_none());
        });
    }

    #[test]
    fn a_profile_tells_chrome_to_keep_webrtc_in_the_proxy_and_load_nothing_ahead() {
        let base = tempfile::tempdir().unwrap();
        let profile = Profile::create_in(base.path()).unwrap();
        assert!(profile.path().starts_with(base.path()));

        // Where Chrome looks for it, and nothing in it but what is meant.
        let written = std::fs::read_to_string(profile.path().join("Default").join("Preferences")).unwrap();
        let preferences: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(
            preferences,
            serde_json::json!({
                "webrtc": {"ip_handling_policy": "disable_non_proxied_udp"},
                "net": {"network_prediction_options": 2},
                "browser": {"enable_spellchecking": false},
            })
        );
    }

    #[test]
    fn chrome_is_told_not_to_ask_its_maker_the_time() {
        // One of the things a Chrome asks for on its own account at every
        // start, and one of the two a setting turns off; the other is in
        // the profile.
        let base = tempfile::tempdir().unwrap();
        let profile = Profile::create_in(base.path()).unwrap();
        let args = chrome_args("https://shop.example/p", &profile, 4321);
        assert!(args.contains(&"--disable-features=NetworkTimeServiceQuerying".to_string()));
        // Chrome takes the last `--disable-features` it is given and drops
        // the others, so there must be the one.
        assert_eq!(args.iter().filter(|a| a.starts_with("--disable-features")).count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_profile_is_its_owners_alone() {
        // As Chrome makes one itself (measured 2026-11-21, Chrome 157: 700
        // on the directory and on `Default`, 600 on `Preferences`). Made by
        // us it must not be any wider: it holds what a render's pages left.
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().unwrap();
        let profile = Profile::create_in(base.path()).unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(profile.path()), 0o700);
        assert_eq!(mode(&profile.path().join("Default")), 0o700);
        assert_eq!(mode(&profile.path().join("Default").join("Preferences")), 0o600);
    }

    #[test]
    fn profiles_made_in_the_same_instant_are_not_one_directory() {
        // Two renders run at once, and the clock in the name ticks in
        // microseconds on a Mac: two profiles of one name would be one
        // directory, and the first render to end would take it from under
        // the other's Chrome.
        let base = tempfile::tempdir().unwrap();
        let made: Vec<PathBuf> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..16)
                .map(|_| {
                    scope.spawn(|| {
                        (0..64)
                            .map(|_| {
                                let profile = Profile::create_in(base.path()).unwrap();
                                let path = profile.path().to_path_buf();
                                // Kept on disk, so a second one of the same
                                // name has something to collide with.
                                std::mem::forget(profile);
                                path
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            threads.into_iter().flat_map(|t| t.join().unwrap()).collect()
        });
        let distinct: std::collections::HashSet<_> = made.iter().collect();
        assert_eq!(distinct.len(), made.len(), "two profiles shared a directory");
    }

    #[test]
    fn a_directory_already_there_is_never_taken_for_a_profile() {
        // The count in the name keeps two renders apart, so no other test
        // would notice this going: a directory that exists is another
        // render's, with a Chrome running on it, and a profile made over it
        // would be removed from under that Chrome when ours is dropped.
        let base = tempfile::tempdir().unwrap();
        let theirs = base.path().join("theirs");
        std::fs::create_dir(&theirs).unwrap();
        std::fs::write(theirs.join("in-use"), "").unwrap();
        let err = private_dir(&theirs).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(theirs.join("in-use").exists());
    }

    #[test]
    fn a_profile_is_made_where_no_temp_directory_is_yet() {
        // Chrome made the whole path when it made the profile itself, so a
        // temp directory nobody had created yet cost nothing. It still must
        // not: every render would fail on it.
        let base = tempfile::tempdir().unwrap();
        let not_yet = base.path().join("tmp").join("scout");
        let profile = Profile::create_in(&not_yet).unwrap();
        assert!(profile.path().join("Default").join("Preferences").is_file());
    }

    #[test]
    fn a_profile_is_gone_when_its_render_is() {
        // However the render ends — Chrome would not start, the deadline
        // dropped it mid-read — the drop takes the directory with it. (What
        // a Chrome still dying can put back afterwards is on `Profile`.)
        let base = tempfile::tempdir().unwrap();
        let profile = Profile::create_in(base.path()).unwrap();
        let path = profile.path().to_path_buf();
        assert!(path.is_dir());
        drop(profile);
        assert!(!path.exists());
    }

    #[test]
    fn a_profile_still_being_written_into_is_gone_once_the_writing_stops() {
        // What a killed Chrome's processes had under way is nothing a render
        // can time: a file written after the directory was removed puts the
        // directory back. So one removal is not the end of it.
        let base = tempfile::tempdir().unwrap();
        let profile = Profile::create_in(base.path()).unwrap();
        let path = profile.path().to_path_buf();
        let late = {
            let default = path.join("Default");
            std::thread::spawn(move || {
                for n in 0..40 {
                    let _ = std::fs::create_dir_all(&default);
                    let _ = std::fs::write(default.join(format!("late-{n}")), "");
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
        };
        drop(profile);
        late.join().unwrap();

        let until = Instant::now() + Duration::from_secs(2);
        while path.exists() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!path.exists(), "what was written late was left there: {:?}", left_in(&path));
    }

    /// The stand-in for a Chrome whose second process goes on writing into
    /// the profile after the first is dead; the script says how.
    #[cfg(unix)]
    const LINGERS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/tools/chrome-that-lingers.sh");

    /// Every path under `dir`, for saying what was left.
    fn left_in(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut look_in = vec![dir.to_path_buf()];
        while let Some(dir) = look_in.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                if entry.path().is_dir() {
                    look_in.push(entry.path());
                }
                found.push(entry.path());
            }
        }
        found
    }

    /// How many files the stand-in has written under `base` so far.
    #[cfg(unix)]
    fn written(base: &std::path::Path) -> usize {
        let wrote = |p: &PathBuf| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("written-"));
        left_in(base).iter().filter(|p| wrote(p)).count()
    }

    /// Waits for `what` to come true, looking every 20 ms, and says whether
    /// it did within `within`.
    #[cfg(unix)]
    async fn comes_true(within: Duration, what: impl Fn() -> bool) -> bool {
        let until = Instant::now() + within;
        while !what() {
            if Instant::now() >= until {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        true
    }

    /// Whether `base` is empty, looked at once the profile's own removing
    /// has certainly ended. Sooner says nothing: for as long as the drop
    /// goes on removing, up to `GIVE_UP`, a directory that a live writer
    /// keeps putting back is away at most looks all the same (the first
    /// version of this looked after half a second, and with the group's
    /// kill taken out the two tests that use it passed 22 and 10 times in
    /// 30, in the review's runs).
    #[cfg(unix)]
    async fn nothing_is_left_in(base: &std::path::Path) -> bool {
        tokio::time::sleep(GIVE_UP + Duration::from_millis(500)).await;
        left_in(base).is_empty()
    }

    /// The command lines of the processes alive that were started on
    /// `profile`: a Chrome and its helpers each carry the path, and so
    /// does every process of the stand-in.
    pub(super) fn alive_on(profile: &std::path::Path) -> Vec<String> {
        let on = profile.display().to_string();
        let lines: Vec<String> = match std::fs::read_dir("/proc") {
            // Linux, with or without a `ps`.
            Ok(processes) => processes
                .flatten()
                .filter_map(|process| std::fs::read(process.path().join("cmdline")).ok())
                .map(|line| String::from_utf8_lossy(&line).replace('\0', " "))
                .collect(),
            Err(_) => {
                let ps = std::process::Command::new("ps").args(["-axww", "-o", "command="]).output().expect("ps");
                String::from_utf8_lossy(&ps.stdout).lines().map(str::to_string).collect()
            }
        };
        lines.into_iter().filter(|line| line.contains(&on)).collect()
    }

    /// The one profile in `base`.
    #[cfg(unix)]
    fn the_profile_in(base: &std::path::Path) -> PathBuf {
        let profiles: Vec<PathBuf> = std::fs::read_dir(base).unwrap().flatten().map(|entry| entry.path()).collect();
        assert_eq!(profiles.len(), 1, "one render, one profile: {profiles:?}");
        profiles[0].clone()
    }

    /// A renderer whose Chrome is the stand-in and whose profiles are made
    /// in `base`.
    #[cfg(unix)]
    fn renderer_that_lingers(base: &std::path::Path) -> Renderer {
        Renderer { profiles: base.to_path_buf(), ..Renderer::new(PathBuf::from(LINGERS)) }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_stand_in_goes_on_writing_when_only_its_first_process_is_killed() {
        // The two tests after this one say a render takes all of its Chrome
        // with it. They say nothing unless the stand-in is a Chrome that
        // does outlive its first process, which is what this one says.
        let base = tempfile::tempdir().unwrap();
        let profile = base.path().join("profile");
        let mut first = tokio::process::Command::new(LINGERS)
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("http://shop.example/")
            .stdout(std::process::Stdio::null())
            // A group of its own only so that the test can stop the rest of
            // it at the end: the kill below is of the one process.
            .process_group(0)
            .spawn()
            .unwrap();
        let group = rustix::process::Pid::from_raw(first.id().unwrap() as i32).unwrap();
        assert!(comes_true(Duration::from_secs(5), || written(base.path()) > 0).await, "the stand-in wrote nothing");

        first.start_kill().unwrap();
        first.wait().await.unwrap();
        let then = written(base.path());
        let went_on = comes_true(Duration::from_secs(2), || written(base.path()) > then + 5).await;
        // And it outlasts removal, which is what a profile's drop does to
        // it: the directory is put back, with more written into it. Removed
        // over and over for a second, not once: what ended the writer was a
        // removal landing between two of its lines, which one removal
        // seldom does (with the script's `true` put back to `:` this
        // passed 15 times in 15 on one removal, and fails on these).
        let removing_until = Instant::now() + Duration::from_secs(1);
        let mut removed = false;
        while Instant::now() < removing_until {
            removed |= std::fs::remove_dir_all(&profile).is_ok();
        }
        let put_back = comes_true(Duration::from_secs(2), || written(base.path()) > 5).await;
        let still_alive = alive_on(&profile).len();
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        assert!(went_on, "nothing more was written once the first process was dead");
        assert!(removed, "the directory could not be removed from under the stand-in");
        assert!(put_back, "the directory was not put back once it had been removed");
        assert!(still_alive > 0, "no process of the stand-in was found by its profile");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_render_cut_short_takes_all_of_its_chrome_and_its_profile_with_it() {
        // The deadline drops a render where it stands. Chrome's first
        // process was killed on that way out and its others were not, and
        // the profile was removed once, under them: 2 of 48 renders cut
        // short left their profile in the temp directory (2026-11-21).
        let base = tempfile::tempdir().unwrap();
        let renderer = renderer_that_lingers(base.path());
        let render = tokio::spawn(async move { renderer.render_inner("http://shop.example/").await });
        assert!(comes_true(Duration::from_secs(5), || written(base.path()) > 0).await, "the stand-in wrote nothing");
        let profile = the_profile_in(base.path());
        assert!(!alive_on(&profile).is_empty(), "no process of the stand-in was found by its profile");

        // Dropped mid-read, as `render`'s timeout drops it.
        render.abort();
        assert!(render.await.unwrap_err().is_cancelled());

        // The second process too, and at once: it has ten seconds of
        // writing ahead of it if nothing kills it.
        assert!(
            comes_true(Duration::from_secs(1), || alive_on(&profile).is_empty()).await,
            "still alive: {:?}",
            alive_on(&profile)
        );
        assert!(nothing_is_left_in(base.path()).await, "left behind: {:?}", left_in(base.path()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_render_that_ends_with_its_page_takes_all_of_its_chrome_with_it_too() {
        // The ordinary way out: the page is printed, Chrome lingers and is
        // killed. Its other processes have as much to say about the profile
        // here as on the deadline's way out.
        let base = tempfile::tempdir().unwrap();
        let renderer = renderer_that_lingers(base.path());
        let render = tokio::spawn(async move { renderer.render_inner("http://shop.example/prints").await });
        assert!(comes_true(Duration::from_secs(5), || written(base.path()) > 0).await, "the stand-in wrote nothing");
        let profile = the_profile_in(base.path());

        // Killed, not waited out: the stand-in's first process goes on for
        // ten seconds if it is left to end by itself, and so does a Chrome.
        let html = tokio::time::timeout(QUIET_AFTER_OUTPUT + Duration::from_secs(3), render)
            .await
            .expect("the render waited for a Chrome it should have killed")
            .unwrap()
            .unwrap();
        assert!(html.contains("printed"), "got: {html}");
        assert!(
            comes_true(Duration::from_secs(1), || alive_on(&profile).is_empty()).await,
            "still alive: {:?}",
            alive_on(&profile)
        );
        assert!(nothing_is_left_in(base.path()).await, "left behind: {:?}", left_in(base.path()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_chrome_is_given_nothing_of_ours_to_read() {
        // In a group of its own a Chrome is no longer in the terminal's
        // foreground, and one that read the terminal would be stopped for
        // it. (Where the tests themselves are given nothing to read this
        // passes whatever the render does.)
        let base = tempfile::tempdir().unwrap();
        let html = renderer_that_lingers(base.path()).render_inner("http://shop.example/prints").await.unwrap();
        assert!(html.contains("reads nothing"), "got: {html}");
    }

    #[test]
    fn a_profile_that_cannot_be_written_is_an_error_not_a_bare_profile() {
        // A Chrome started on a profile without the preferences is a Chrome
        // that sends UDP where it likes. A file where the directory should
        // go cannot hold one, whoever runs the test.
        let base = tempfile::tempdir().unwrap();
        let file = base.path().join("a-file");
        std::fs::write(&file, "").unwrap();
        assert!(Profile::create_in(&file).is_err());
    }

    #[test]
    fn a_renderer_opens_the_public_internet_and_nothing_else() {
        // The guard is a field so that a live test can point a real Chrome
        // at a mock on loopback; nothing else may ever set it.
        let renderer = Renderer::new(PathBuf::from("/usr/bin/chromium"));
        assert_eq!(renderer.guard, crate::net_guard::Guard::PUBLIC_ONLY);
    }

    #[tokio::test]
    async fn a_failed_render_hands_its_slot_back() {
        // A leaked permit is worse than no limit at all: the renderer would
        // work until the first failure and be wedged shut afterwards.
        let renderer = Renderer::new(PathBuf::from("/definitely/not/chrome"));
        assert!(renderer.render("https://example.com").await.is_err());
        assert_eq!(renderer.slots.available_permits(), MAX_CONCURRENT_RENDERS);
    }

    #[tokio::test(start_paused = true)]
    async fn a_render_with_no_slot_free_gives_up_rather_than_starting_another_chrome() {
        // Each render is a Chrome process of a few hundred megabytes. Ten at
        // once is several gigabytes, which is how this box runs out of memory
        // rather than how it serves ten people.
        let renderer = Renderer::new(PathBuf::from("/definitely/not/chrome"));
        let held: Vec<_> = (0..MAX_CONCURRENT_RENDERS)
            .map(|_| renderer.slots.try_acquire().expect("a free slot"))
            .collect();
        assert_eq!(renderer.slots.available_permits(), 0);

        let err = renderer.render("https://example.com").await.unwrap_err();
        assert!(matches!(err, BrowserError::Busy), "got: {err}");

        drop(held);
        // And the queue reopens the moment a slot frees, rather than staying
        // shut because something counted wrong.
        assert_eq!(renderer.slots.available_permits(), MAX_CONCURRENT_RENDERS);
    }

    #[tokio::test]
    async fn what_is_not_a_web_address_is_refused_before_any_chrome() {
        // `--dump-dom` prints whatever Chrome is pointed at, a file
        // included, and the browser pod is asked over HTTP: a render does
        // not take its caller's word that the address is a web page's. The
        // Chrome here does not exist, so any answer but a refusal would
        // have been a failure to start it.
        let renderer = Renderer::new(PathBuf::from("/definitely/not/chrome"));
        for (url, why) in [
            ("file:///etc/passwd", "file:"),
            ("FILE:///etc/passwd", "file:"),
            ("chrome://version", "chrome:"),
            ("view-source:https://example.com/", "view-source:"),
            ("data:text/html,<p>x</p>", "data:"),
            ("javascript:alert(1)", "javascript:"),
            ("about:blank", "about:"),
            ("not an address", "not a web address"),
            ("", "not a web address"),
        ] {
            match renderer.render(url).await {
                Err(BrowserError::Refused(said)) => assert!(said.contains(why), "{url}: {said}"),
                other => panic!("{url}: {other:?}"),
            }
        }
        assert_eq!(renderer.slots.available_permits(), MAX_CONCURRENT_RENDERS, "a refusal takes no slot");
    }

    // ----------------------------------------- a render asked of the pod

    use wiremock::matchers::{any, body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A renderer that asks the pod standing at `pod`.
    fn asking(pod: &MockServer) -> Renderer {
        Renderer::remote(&url::Url::parse(&pod.uri()).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn a_render_asked_of_the_pod_is_the_page_the_pod_rendered() {
        let pod = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/render"))
            .and(body_json(serde_json::json!({ "url": "https://shop.example/p/1" })))
            .respond_with(ResponseTemplate::new(200).set_body_raw("<html>rendered</html>", "text/html; charset=utf-8"))
            .expect(1)
            .mount(&pod)
            .await;
        assert_eq!(asking(&pod).render("https://shop.example/p/1").await.unwrap(), "<html>rendered</html>");
    }

    #[tokio::test]
    async fn what_the_pod_says_went_wrong_is_what_the_render_says() {
        let said = |status: u16, error: &str| {
            ResponseTemplate::new(status).set_body_json(serde_json::json!({ "error": error }))
        };
        for (answer, expected) in [
            (said(503, "too many pages are already rendering"), "busy"),
            (said(504, "rendering timed out"), "timeout"),
            (said(422, "only a web page is rendered, and that address is file:"), "refused"),
            (said(502, "headless browser failed: no Chrome"), "502"),
            (ResponseTemplate::new(500), "500"),
            (ResponseTemplate::new(404), "404"),
        ] {
            let pod = MockServer::start().await;
            Mock::given(path("/render")).respond_with(answer).mount(&pod).await;
            let err = asking(&pod).render("https://shop.example/p/1").await.unwrap_err();
            match (expected, &err) {
                ("busy", BrowserError::Busy) | ("timeout", BrowserError::Timeout) => {}
                // The pod's own sentence, for whoever asked: it says what was wrong with the address.
                ("refused", BrowserError::Refused(why)) => assert_eq!(why, "only a web page is rendered, and that address is file:"),
                (status, BrowserError::Launch(why)) if why.contains(status) => {}
                _ => panic!("{expected}: {err:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_pod_that_answers_with_a_redirect_is_not_followed() {
        // Were the pod taken over, a redirect would point Scout's own
        // client, which no guard holds, anywhere in the cluster.
        let (pod, elsewhere) = (MockServer::start().await, MockServer::start().await);
        Mock::given(any()).respond_with(ResponseTemplate::new(200).set_body_string("followed")).expect(0).mount(&elsewhere).await;
        Mock::given(path("/render"))
            .respond_with(ResponseTemplate::new(307).insert_header("location", format!("{}/metrics", elsewhere.uri())))
            .mount(&pod)
            .await;
        let err = asking(&pod).render("https://shop.example/p/1").await.unwrap_err();
        assert!(matches!(&err, BrowserError::Launch(why) if why.contains("307")), "{err:?}");
    }

    #[tokio::test]
    async fn a_page_from_the_pod_is_read_up_to_the_cap_and_no_further() {
        // The pod is where a page that took a Chrome over would be, so its
        // answer is that page's to make, length and all.
        let pod = MockServer::start().await;
        Mock::given(path("/render")).respond_with(ResponseTemplate::new(200).set_body_raw("x".repeat(1000), "text/html")).mount(&pod).await;
        let capped = |cap| {
            let mut renderer = asking(&pod);
            if let How::Remote(remote) = &mut renderer.how {
                remote.cap = cap;
            }
            renderer
        };
        assert_eq!(capped(1000).render("https://shop.example/p/1").await.unwrap().len(), 1000);
        let err = capped(999).render("https://shop.example/p/1").await.unwrap_err();
        assert!(matches!(&err, BrowserError::Launch(why) if why.contains("999 bytes")), "{err:?}");
        assert_eq!(MAX_REMOTE_PAGE, 16 * 1024 * 1024);
    }

    #[tokio::test(start_paused = true)]
    async fn a_pod_that_never_answers_is_a_timeout_at_the_renders_deadline() {
        // A pod that takes the request and says nothing, as a wedged one would.
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = silent.local_addr().unwrap();
        let held = tokio::spawn(async move {
            let mut open = Vec::new();
            while let Ok((socket, _)) = silent.accept().await {
                open.push(socket);
            }
        });
        let renderer = Renderer::remote(&url::Url::parse(&format!("http://{at}")).unwrap()).unwrap();
        let started = tokio::time::Instant::now();
        let err = renderer.render("https://shop.example/p/1").await.unwrap_err();
        assert!(matches!(err, BrowserError::Timeout), "{err:?}");
        assert!(started.elapsed() <= RENDER_TIMEOUT + Duration::from_secs(1), "{:?}", started.elapsed());
        held.abort();
    }

    #[tokio::test]
    async fn a_render_asked_of_the_pod_is_refused_what_a_local_one_is_without_asking_the_pod() {
        // The pod checks too. This is the first of the two checks, not
        // the only one, and the pod is not asked for what it would refuse.
        let pod = MockServer::start().await;
        Mock::given(any()).respond_with(ResponseTemplate::new(200)).expect(0).mount(&pod).await;
        let renderer = asking(&pod);
        for url in ["file:///etc/passwd", "http://127.0.0.1:9090/metrics", "http://169.254.169.254/"] {
            assert!(matches!(renderer.render(url).await, Err(BrowserError::Refused(_))), "{url}");
        }
    }

    #[tokio::test]
    async fn a_render_asked_of_the_pod_waits_for_one_of_its_own_slots_first() {
        // Scout's two slots are backpressure, so that a burst of renders
        // queues here rather than as connections held open to the pod.
        let pod = MockServer::start().await;
        Mock::given(any()).respond_with(ResponseTemplate::new(200).set_body_string("<html>x</html>")).expect(0).mount(&pod).await;
        let renderer = asking(&pod);
        let held: Vec<_> = (0..MAX_CONCURRENT_RENDERS).map(|_| renderer.slots.try_acquire().expect("a free slot")).collect();
        let waiting = tokio::time::timeout(Duration::from_millis(300), renderer.render("https://shop.example/p/1")).await;
        assert!(waiting.is_err(), "a render went to the pod with every slot taken");
        drop(held);
    }

    #[tokio::test]
    async fn an_address_off_the_public_internet_is_refused_before_any_chrome() {
        // The proxy would refuse each of these too, once Chrome asked; but
        // Chrome would then be running, and print its own error page as if
        // it were the page.
        let renderer = Renderer::new(PathBuf::from("/definitely/not/chrome"));
        for url in [
            "http://127.0.0.1:9090/metrics",
            "http://localhost:8080/",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.43.0.1/",
            "http://[::1]/",
            "https://[::ffff:127.0.0.1]/",
        ] {
            match renderer.render(url).await {
                Err(BrowserError::Refused(said)) => assert_eq!(said, crate::net_guard::NOT_PUBLIC, "{url}"),
                other => panic!("{url}: {other:?}"),
            }
        }
    }

    /// Sets an env var for the duration of `f`. Tests run in threads, so this
    /// stays in one place rather than being scattered.
    fn temp_env_var(key: &str, value: &str, f: impl FnOnce()) {
        let previous = std::env::var(key).ok();
        // SAFETY: single-threaded assertion inside f; restored right after.
        unsafe { std::env::set_var(key, value) };
        f();
        match previous {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }
}

#[cfg(test)]
mod live {
    //! Ignored by default: needs a real Chrome and network.
    //!
    //! The `a_page_…` tests need the Chrome only. Their page is served from a
    //! mock on loopback and their renderer's proxy opens that mock and
    //! nothing else, so they say what a page can do from inside a render
    //! with no connection made to anything but this machine. (The names
    //! Chrome asks the proxy for on its own account are still looked up
    //! before they are refused.)
    //!
    //! WebRTC is shut twice, by the profile's preference and by a switch.
    //! The `a_page_…` tests run the renderer as it is, with both, and say
    //! the pair holds; `either_telling_alone_…` takes one out at a time,
    //! and `a_chrome_told_neither_way_…` both, which is what shows the
    //! others could have failed. They aim at loopback and at whatever other
    //! addresses the machine has (`aims`).
    //!
    //! `scripts/check-render-chromium.sh` runs all of them but
    //! `renders_a_walled_page` against the Chromium the image ships, which
    //! is not a laptop's Chrome. Measured there 2026-11-22, Debian's
    //! Chromium 156, with a private address and a link-local one beside
    //! loopback: told neither way, every listener was reached; told either
    //! way alone, or both, none was and no candidate was gathered.
    use super::tests::alive_on;
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use tokio::net::UdpSocket;
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The wait every test page makes before it writes down what it saw.
    ///
    /// `--dump-dom` prints the page when its virtual time runs out, and
    /// virtual time is not time. Measured 2026-11-20, Chrome 157 with the
    /// render's flags and a STUN server named on loopback: a page that only
    /// opens a peer connection is printed with ICE not begun (state `new`,
    /// nothing sent); one that then waits five seconds on a timer is printed
    /// 0.1 s later with ICE `gathering` and still nothing sent, since timers
    /// are skipped through; one that waits on a request the server sits on
    /// has ICE stand still until the answer comes. Gathering wants both
    /// kinds of time, so a round here is a request held 200 ms and then a
    /// timer — with that the first datagram left a quarter of a second after
    /// the page was served. Five rounds whatever happens, so that "nothing
    /// came" is said after a second and not at once; up to twenty-five while
    /// `done()` is false.
    const WAIT: &str = "const wait = async (done) => {
  for (let i = 0; i < 25 && (i < 5 || !done()); i++) {
    await fetch('/tick').catch(() => {});
    await new Promise(r => setTimeout(r, 100));
  }
};";

    /// A page that opens a peer connection with `CONFIG`, lets ICE gather
    /// and writes what came of it where the dump shows it. The wait is
    /// begun first, so its held request is out before the page awaits
    /// anything of WebRTC's: a page with enough such awaits ahead of its
    /// first request can be printed before it reaches the wait.
    const ICE_PAGE: &str = r#"<html><head><title>ice</title></head><body><pre id="out">ice:start</pre><script>
WAIT
(async () => {
  const candidates = [];
  let pc;
  const waited = wait(() => pc && pc.iceGatheringState === 'complete');
  pc = new RTCPeerConnection(CONFIG);
  pc.onicecandidate = e => { if (e.candidate) candidates.push(e.candidate.candidate); };
  pc.createDataChannel('x');
  await pc.setLocalDescription(await pc.createOffer());
  await waited;
  document.getElementById('out').textContent =
    'ice:done state=' + pc.iceGatheringState + ' candidates=' + candidates.length + ' ' + candidates.join(' | ');
})();
</script></body></html>"#;

    /// A page that needs no server to have Chrome send UDP: it pairs two
    /// connections of its own and hands one a remote candidate at each
    /// address in `PEERS`, which Chrome then checks for a peer.
    const PEER_PAGE: &str = r#"<html><head><title>peer</title></head><body><pre id="out">peer:start</pre><script>
WAIT
(async () => {
  let note = '';
  const waited = wait(() => false);
  try {
    const a = new RTCPeerConnection(), b = new RTCPeerConnection();
    a.createDataChannel('x');
    const offer = await a.createOffer();
    await a.setLocalDescription(offer);
    await b.setRemoteDescription(offer);
    const answer = await b.createAnswer();
    await b.setLocalDescription(answer);
    await a.setRemoteDescription(answer);
    let added = 0;
    for (const [address, port] of PEERS) {
      await a.addIceCandidate({candidate: `candidate:${added + 2} 1 udp 2113937151 ${address} ${port} typ host`, sdpMid: '0', sdpMLineIndex: 0});
      added++;
    }
    note = ' candidates=' + added;
  } catch (e) { note = ' error=' + e; }
  await waited;
  document.getElementById('out').textContent = 'peer:done' + note;
})();
</script></body></html>"#;

    /// Which of the two ways Chrome is told to keep WebRTC in the proxy a
    /// test takes out of its render. `Nothing` is the render as it is.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Without {
        Nothing,
        Switch,
        Preference,
        Both,
    }

    /// A renderer as production's, but for two things: its Chrome reaches
    /// `mock` and nothing else, and it is told what `without` leaves.
    fn renderer(mock: std::net::SocketAddr, without: Without) -> Renderer {
        let before: BeforeChrome = Arc::new(move |profile, args| {
            if matches!(without, Without::Preference | Without::Both) {
                // The setting taken out, what else the profile says left in.
                let mut preferences: serde_json::Value = serde_json::from_str(PREFERENCES).unwrap();
                preferences.as_object_mut().unwrap().remove("webrtc").expect("the preference is there to take out");
                std::fs::write(profile.join("Default").join("Preferences"), preferences.to_string()).unwrap();
            }
            if matches!(without, Without::Switch | Without::Both) {
                let before = args.len();
                args.retain(|arg| !arg.starts_with("--webrtc-ip-handling-policy"));
                assert_eq!(args.len(), before - 1, "the switch is there to take out");
            }
        });
        Renderer { before_chrome: Some(before), ..Renderer::reaching_only(find_chrome().expect("chrome"), mock) }
    }

    /// Renders `page` as production would, but for the guard and for what
    /// `without` takes out: Chrome reaches the mock that serves the page
    /// and nothing else.
    async fn render_without(server: &MockServer, page: &str, without: Without) -> String {
        Mock::given(path("/tick"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(200)))
            .mount(server)
            .await;
        Mock::given(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(page.replace("WAIT", WAIT), "text/html"))
            .mount(server)
            .await;
        renderer(*server.address(), without).render(&format!("{}/", server.uri())).await.unwrap()
    }

    async fn render_from_loopback(server: &MockServer, page: &str) -> String {
        render_without(server, page, Without::Nothing).await
    }

    /// What the test page wrote into its `<pre>`.
    fn said(html: &str) -> &str {
        html.split_once(r#"<pre id="out">"#)
            .and_then(|(_, rest)| rest.split_once("</pre>"))
            .map(|(text, _)| text)
            .unwrap_or("(no page)")
    }

    /// Whether anything was sent to `socket` during the render; the kernel
    /// keeps a datagram until it is read.
    async fn reached(socket: &UdpSocket) -> bool {
        let mut buf = [0u8; 64];
        tokio::time::timeout(Duration::from_millis(100), socket.recv_from(&mut buf)).await.is_ok()
    }

    /// The addresses of this machine a page is aimed at: loopback, and the
    /// ones that are not — the address the machine's route out leaves from,
    /// which is a private one behind a router or in a container, and any
    /// named in `SCOUT_PROBE_UDP_ADDRS`, which is how the image's check
    /// hands over a link-local address it has given the container.
    fn aims() -> Vec<IpAddr> {
        let mut aims = vec![IpAddr::from(Ipv4Addr::LOCALHOST)];
        // Connecting a UDP socket sends nothing: it asks the system which
        // address it would send from. 192.0.2.1 is kept for documentation.
        let out = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .and_then(|socket| socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).and_then(|()| socket.local_addr()));
        if let Ok(out) = out {
            aims.push(out.ip());
        }
        let named = std::env::var("SCOUT_PROBE_UDP_ADDRS").unwrap_or_default();
        for named in named.split(',').map(str::trim).filter(|named| !named.is_empty()) {
            aims.push(named.parse().unwrap_or_else(|_| panic!("SCOUT_PROBE_UDP_ADDRS names {named:?}, which is no address")));
        }
        let mut seen = Vec::new();
        aims.retain(|ip| !ip.is_unspecified() && !seen.contains(ip) && {
            seen.push(*ip);
            true
        });
        aims
    }

    /// What kind of address `ip` is, for saying what a test measured.
    fn kind(ip: IpAddr) -> &'static str {
        match ip {
            _ if ip.is_loopback() => "loopback",
            IpAddr::V4(v4) if v4.is_link_local() => "link-local",
            IpAddr::V4(v4) if v4.is_private() => "private",
            IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 == 0xfe80 => "link-local",
            IpAddr::V6(v6) if v6.segments()[0] & 0xfe00 == 0xfc00 => "private",
            _ => "neither loopback, private nor link-local",
        }
    }

    /// A UDP listener a page is pointed at.
    struct Listener {
        what: String,
        socket: UdpSocket,
    }

    impl Listener {
        /// Bound on `ip`, and shown to hear what is sent to it there:
        /// silence from a listener that hears nothing would say nothing.
        async fn on(ip: IpAddr, role: &str) -> Self {
            let socket = UdpSocket::bind((ip, 0)).await.unwrap_or_else(|e| panic!("no listener on {ip}: {e}"));
            Self::heard(format!("{ip} ({}) {role}", kind(ip)), socket).await
        }

        async fn heard(what: String, socket: UdpSocket) -> Self {
            let at = socket.local_addr().unwrap();
            let from = UdpSocket::bind((at.ip(), 0)).await.unwrap();
            from.send_to(b"heard", at).await.unwrap();
            assert!(reached(&socket).await, "the listener on {what} does not hear what is sent to it");
            Self { what, socket }
        }

        fn port(&self) -> u16 {
            self.socket.local_addr().unwrap().port()
        }

        /// Its address as a URL writes one.
        fn host(&self) -> String {
            match self.socket.local_addr().unwrap().ip() {
                IpAddr::V4(ip) => ip.to_string(),
                IpAddr::V6(ip) => format!("[{ip}]"),
            }
        }
    }

    /// Which of `listeners` something was sent to during the render, and
    /// which nothing was.
    async fn those_reached(listeners: &[Listener]) -> (Vec<String>, Vec<String>) {
        let (mut got, mut missed) = (Vec::new(), Vec::new());
        for listener in listeners {
            if reached(&listener.socket).await {
                got.push(listener.what.clone());
            } else {
                missed.push(listener.what.clone());
            }
        }
        (got, missed)
    }

    /// What came of a page that aimed UDP at listeners: which it reached,
    /// which it did not, and what the page wrote.
    struct Udp {
        reached: Vec<String>,
        missed: Vec<String>,
        said: String,
    }

    /// Renders a page that names a STUN and a TURN server on every address
    /// in [`aims`], and a STUN server at `localhost` on each loopback, which
    /// only a resolver on this machine can turn into an address.
    async fn udp_to_the_servers_a_page_names(without: Without) -> Udp {
        let (mut listeners, mut stun, mut turn) = (Vec::new(), Vec::new(), Vec::new());
        for ip in aims() {
            let listener = Listener::on(ip, "as a STUN server").await;
            stun.push(format!("stun:{}:{}", listener.host(), listener.port()));
            listeners.push(listener);
            let listener = Listener::on(ip, "as a TURN server").await;
            turn.push(format!("turn:{}:{}?transport=udp", listener.host(), listener.port()));
            listeners.push(listener);
        }
        let named = Listener::on(Ipv4Addr::LOCALHOST.into(), "as localhost").await;
        stun.push(format!("stun:localhost:{}", named.port()));
        if let Ok(socket) = UdpSocket::bind((Ipv6Addr::LOCALHOST, named.port())).await {
            listeners.push(Listener::heard("::1 (loopback) as localhost".into(), socket).await);
        }
        listeners.push(named);
        println!("LIVE udp aimed at {:?}", listeners.iter().map(|l| &l.what).collect::<Vec<_>>());
        let config = serde_json::json!({"iceServers": [{"urls": stun}, {"urls": turn, "username": "u", "credential": "c"}]});

        let server = MockServer::start().await;
        let html = render_without(&server, &ICE_PAGE.replace("CONFIG", &config.to_string()), without).await;
        let (reached, missed) = those_reached(&listeners).await;
        Udp { reached, missed, said: said(&html).to_string() }
    }

    /// Renders a page that tells one of its own connections its peer is at
    /// each address in [`aims`].
    async fn udp_to_the_peers_a_page_names(without: Without) -> Udp {
        let mut listeners = Vec::new();
        for ip in aims() {
            listeners.push(Listener::on(ip, "as a peer").await);
        }
        println!("LIVE udp aimed at {:?}", listeners.iter().map(|l| &l.what).collect::<Vec<_>>());
        let peers: Vec<_> = listeners
            .iter()
            .map(|l| serde_json::json!([l.socket.local_addr().unwrap().ip().to_string(), l.port()]))
            .collect();

        let server = MockServer::start().await;
        let page = PEER_PAGE.replace("PEERS", &serde_json::Value::from(peers).to_string());
        let html = render_without(&server, &page, without).await;
        let (reached, missed) = those_reached(&listeners).await;
        Udp { reached, missed, said: said(&html).to_string() }
    }

    /// What a page with no server named is handed for candidates.
    async fn candidates_a_page_gathers(without: Without) -> String {
        let server = MockServer::start().await;
        said(&render_without(&server, &ICE_PAGE.replace("CONFIG", ""), without).await).to_string()
    }

    #[tokio::test]
    #[ignore]
    async fn a_page_cannot_send_udp_past_the_guard() {
        // The proxy carries TCP, and WebRTC speaks UDP: a page that names a
        // STUN or a TURN server has Chrome send it a datagram from a socket
        // of its own. Listeners nothing should reach, on loopback and on
        // the machine's other addresses.
        let Udp { reached, said, .. } = udp_to_the_servers_a_page_names(Without::Nothing).await;
        assert!(reached.is_empty(), "a datagram went round the proxy to {reached:?}; the page said: {said}");
        // Silence only counts from a script that ran to its end.
        assert!(said.starts_with("ice:done "), "the page's script did not finish: {said}");
    }

    #[tokio::test]
    #[ignore]
    async fn a_page_cannot_send_udp_to_a_peer_it_names() {
        // No server is needed for this one, so a policy that only stopped
        // Chrome asking servers would leave it open: the page tells one of
        // its own connections that its peer is at addresses of the page's
        // choosing, and Chrome sends a check to each to see.
        let Udp { reached, said, .. } = udp_to_the_peers_a_page_names(Without::Nothing).await;
        assert!(reached.is_empty(), "a check went round the proxy to {reached:?}; the page said: {said}");
        // The candidates were taken and the wait ran out, or nothing was tried.
        assert_eq!(said, format!("peer:done candidates={}", aims().len()));
    }

    #[tokio::test]
    #[ignore]
    async fn a_page_gathers_no_ice_candidates() {
        // With no STUN server nothing is sent, but the candidates Chrome
        // gathers are handed to the page's script, and they are how a page
        // reads the addresses of the machine it runs on.
        let said = candidates_a_page_gathers(Without::Nothing).await;
        assert!(said.starts_with("ice:done state=complete candidates=0 "), "the page said: {said}");
    }

    #[tokio::test]
    #[ignore]
    async fn a_chrome_told_neither_way_does_send_a_pages_udp() {
        // What the three tests above are worth. Each says "nothing came",
        // which a Chrome with no WebRTC in it would say as well, and a
        // listener the page could never have reached. So the same pages on
        // the same Chrome with both tellings taken out: every listener the
        // tests aim at has to be reached, and a page has to be handed
        // candidates. If this fails on some Chrome, those tests say nothing
        // about that Chrome.
        let servers = udp_to_the_servers_a_page_names(Without::Both).await;
        let peers = udp_to_the_peers_a_page_names(Without::Both).await;
        let candidates = candidates_a_page_gathers(Without::Both).await;
        println!(
            "LIVE told nothing: servers reached {:?}; peers reached {:?}; candidates: {candidates}",
            servers.reached, peers.reached
        );
        assert!(servers.missed.is_empty(), "nothing reached {:?}, shut or not; the page said: {}", servers.missed, servers.said);
        assert!(peers.missed.is_empty(), "nothing reached {:?}, shut or not; the page said: {}", peers.missed, peers.said);
        assert!(!candidates.starts_with("ice:done state=complete candidates=0 "), "no candidate, shut or not: {candidates}");
    }

    #[tokio::test]
    #[ignore]
    async fn either_telling_alone_keeps_a_pages_udp_in() {
        // Told twice because a Chrome that does not know one way ignores it
        // in silence. That is only cover if each way holds without the
        // other on the Chrome that runs, so each is taken out in turn.
        for (left, without) in [("the switch", Without::Preference), ("the preference", Without::Switch)] {
            let Udp { reached, said, .. } = udp_to_the_servers_a_page_names(without).await;
            assert!(reached.is_empty(), "with {left} alone a datagram reached {reached:?}; the page said: {said}");
            assert!(said.starts_with("ice:done "), "with {left} alone the page's script did not finish: {said}");
            let Udp { reached, said, .. } = udp_to_the_peers_a_page_names(without).await;
            assert!(reached.is_empty(), "with {left} alone a check reached {reached:?}; the page said: {said}");
            let candidates = candidates_a_page_gathers(without).await;
            assert!(
                candidates.starts_with("ice:done state=complete candidates=0 "),
                "with {left} alone the page said: {candidates}"
            );
            println!("LIVE {left} alone: nothing reached, no candidate");
        }
    }

    #[tokio::test]
    #[ignore]
    async fn a_page_cannot_have_chrome_load_ahead_of_it() {
        // Speculation rules have Chrome fetch, or wholly render, addresses
        // the page names before anyone goes there: one beside the page, one
        // on a listener the guard refuses.
        let server = MockServer::start().await;
        let inside = MockServer::start().await;
        let rules = serde_json::json!({
            "prefetch": [{"source": "list", "urls": ["/ahead", format!("{}/ahead", inside.uri())]}],
            "prerender": [{"source": "list", "urls": ["/ahead-whole"]}],
        });
        let page = format!(
            r#"<html><head><title>ahead</title><script type="speculationrules">{rules}</script></head>
<body><pre id="out">start</pre><script>
WAIT
(async () => {{ await wait(() => true); document.getElementById('out').textContent = 'waited'; }})();
</script></body></html>"#
        );
        let html = render_from_loopback(&server, &page).await;
        assert_eq!(said(&html), "waited");

        let asked = |requests: Option<Vec<wiremock::Request>>| -> Vec<String> {
            let requests = requests.expect("the mock records what it is asked");
            requests.iter().map(|r| r.url.path().to_string()).collect()
        };
        let inside = asked(inside.received_requests().await);
        assert!(inside.is_empty(), "a load ahead got past the guard: {inside:?}");
        let ahead: Vec<String> =
            asked(server.received_requests().await).into_iter().filter(|p| p.starts_with("/ahead")).collect();
        assert!(ahead.is_empty(), "Chrome loaded ahead of the page: {ahead:?}");
    }

    /// What a Chrome started as a render starts it still asks its proxy for
    /// on its own account, at every start, with nothing found that turns it
    /// off. Measured 2026-11-22, three starts of each and more:
    ///
    /// - Debian's Chromium 156, the image's: a list of the accounts signed
    ///   in (`accounts.google.com`), and whether one component it wants has
    ///   an update (`update.googleapis.com`). `--disable-component-update`,
    ///   `--disable-background-networking`, `signin.allowed` false in the
    ///   profile and a dozen switches more changed neither; nor did
    ///   starting the binary without Debian's launcher script, which adds
    ///   switches and API keys of its own. Let through, as the guard lets
    ///   them through, each answers with a few kilobytes and nothing is
    ///   downloaded.
    /// - Google's Chrome 157, a laptop's: those two, and `www.google.com`,
    ///   `www.gstatic.com` and `clientservices.googleapis.com`. There
    ///   `--disable-component-update` does end the update check and
    ///   `--disable-background-networking` the last of the five. The render
    ///   passes neither: on the Chromium that runs they change nothing.
    ///
    /// Two switches do silence the first pair, by sending Chrome's sign-in
    /// and its updater to an address of the caller's choosing
    /// (`--gaia-url`, `--component-updater=url-source=`), which is a larger
    /// thing to do to a browser than the asking is.
    const STILL_ASKED: [&str; 5] = [
        "accounts.google.com",
        "update.googleapis.com",
        "www.google.com",
        "www.gstatic.com",
        "clientservices.googleapis.com",
    ];

    /// A SOCKS5 proxy that opens nothing and keeps the `host:port` of each
    /// thing it is asked to open.
    async fn a_proxy_that_only_notes() -> (u16, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let notes = asked.clone();
        tokio::spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                let notes = notes.clone();
                tokio::spawn(async move {
                    // The greeting, then the request: RFC 1928, as the
                    // guard's proxy reads them.
                    let mut head = [0u8; 2];
                    conn.read_exact(&mut head).await.ok()?;
                    conn.read_exact(&mut vec![0u8; head[1] as usize]).await.ok()?;
                    conn.write_all(&[5, 0]).await.ok()?;
                    let mut request = [0u8; 4];
                    conn.read_exact(&mut request).await.ok()?;
                    let host = match request[3] {
                        1 => {
                            let mut ip = [0u8; 4];
                            conn.read_exact(&mut ip).await.ok()?;
                            IpAddr::from(ip).to_string()
                        }
                        4 => {
                            let mut ip = [0u8; 16];
                            conn.read_exact(&mut ip).await.ok()?;
                            IpAddr::from(ip).to_string()
                        }
                        _ => {
                            let mut name = vec![0u8; conn.read_u8().await.ok()? as usize];
                            conn.read_exact(&mut name).await.ok()?;
                            String::from_utf8_lossy(&name).into_owned()
                        }
                    };
                    let port = conn.read_u16().await.ok()?;
                    notes.lock().unwrap().push(format!("{host}:{port}"));
                    // "Not allowed", which is what the guard answers to
                    // what it refuses.
                    conn.write_all(&[5, 2, 0, 1, 0, 0, 0, 0, 0, 0]).await.ok()
                });
            }
        });
        (port, asked)
    }

    #[tokio::test]
    #[ignore]
    async fn a_chrome_asks_its_proxy_for_less_of_its_own() {
        // A Chrome asks for its maker's hosts before it asks for the page,
        // and production's proxy opens them: they are public. Two of those
        // askings have a setting that ends them — the time
        // (`clients2.google.com`), by a switch, and a spelling dictionary
        // (`redirector.gvt1.com`, Debian's Chromium), by the profile — and
        // this says they are ended, and that nothing has joined the rest.
        //
        // Chrome is sent to a proxy that opens nothing and notes what it
        // is asked; the page's own server it dials itself, so what the
        // proxy hears is Chrome's own, and one request the page makes to
        // show the proxy does hear.
        let server = MockServer::start().await;
        let (noting, asked) = a_proxy_that_only_notes().await;
        let page = r#"<html><head><title>asks</title></head><body><pre id="out">start</pre><script>
WAIT
(async () => {
  const heard = fetch('http://heard.example/').catch(() => {});
  await wait(() => false);
  await heard;
  document.getElementById('out').textContent = 'waited';
})();
</script></body></html>"#;
        Mock::given(path("/tick"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(200)))
            .mount(&server)
            .await;
        Mock::given(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(page.replace("WAIT", WAIT), "text/html"))
            .mount(&server)
            .await;
        let mock = *server.address();
        let before: BeforeChrome = Arc::new(move |_, args| {
            for arg in args.iter_mut() {
                if arg.starts_with("--proxy-server=") {
                    *arg = format!("--proxy-server=socks5://127.0.0.1:{noting}");
                } else if arg.starts_with("--proxy-bypass-list=") {
                    *arg = format!("--proxy-bypass-list=<-loopback>;{mock}");
                }
            }
        });
        // Reaching only the mock, so that the render's own check lets its
        // page be asked for. The proxy that guard serves is not the one this
        // Chrome is sent through: the line above puts the noting one there.
        let renderer = Renderer { before_chrome: Some(before), ..Renderer::reaching_only(find_chrome().expect("chrome"), mock) };
        let html = renderer.render(&format!("{}/", server.uri())).await.unwrap();
        assert_eq!(said(&html), "waited");

        let mut hosts: Vec<String> = asked
            .lock()
            .unwrap()
            .iter()
            .map(|asked| asked.rsplit_once(':').map_or(asked.as_str(), |(host, _)| host).to_string())
            .collect();
        hosts.sort();
        hosts.dedup();
        println!("LIVE asked of the proxy: {hosts:?}");
        assert!(hosts.iter().any(|host| host == "heard.example"), "the proxy did not hear the page's own request: {hosts:?}");
        let new: Vec<_> = hosts.iter().filter(|host| *host != "heard.example" && !STILL_ASKED.contains(&host.as_str())).collect();
        assert!(new.is_empty(), "Chrome asked on its own account for {new:?}, beside {STILL_ASKED:?}");
    }

    #[tokio::test]
    #[ignore]
    async fn renders_cut_short_leave_no_chrome_and_no_profile() {
        // What `a_render_cut_short_takes_all_of_its_chrome_and_its_profile_with_it`
        // says of a stand-in, of a real Chrome: sixteen renders dropped as
        // the deadline drops one, from a fifth of a second in to four
        // seconds in, on a page that is never done. A second after each,
        // no process started on its profile is alive and the profile is
        // gone; and at the end nothing has been put back.
        let server = MockServer::start().await;
        let page = "<html><head><title>never done</title></head><body><script>
(async () => { for (;;) { await fetch('/tick').catch(() => {}); } })();
</script></body></html>";
        Mock::given(path("/tick"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(200)))
            .mount(&server)
            .await;
        Mock::given(path("/")).respond_with(ResponseTemplate::new(200).set_body_raw(page, "text/html")).mount(&server).await;
        let base = tempfile::tempdir().unwrap();
        let chrome = find_chrome().expect("chrome");

        let mut left = Vec::new();
        let (mut helpers, mut soon) = (0, 0);
        for cut in 0..16u64 {
            let after = Duration::from_millis(200 + cut * 250);
            let renderer = Renderer {
                profiles: base.path().to_path_buf(),
                ..Renderer::reaching_only(chrome.clone(), *server.address())
            };
            let url = format!("{}/", server.uri());
            let render = tokio::spawn(async move { renderer.render_inner(&url).await });
            tokio::time::sleep(after).await;
            let profiles: Vec<PathBuf> = std::fs::read_dir(base.path()).unwrap().flatten().map(|entry| entry.path()).collect();
            assert_eq!(profiles.len(), 1, "one render, one profile: {profiles:?}");
            helpers += alive_on(&profiles[0]).len();
            render.abort();
            assert!(render.await.unwrap_err().is_cancelled(), "the render ended before it was cut, {after:?} in");

            tokio::time::sleep(Duration::from_millis(100)).await;
            soon += alive_on(&profiles[0]).len();
            tokio::time::sleep(Duration::from_millis(900)).await;
            let alive = alive_on(&profiles[0]);
            if !alive.is_empty() || profiles[0].exists() {
                left.push(format!("cut {after:?} in: profile there: {}, alive: {alive:?}", profiles[0].exists()));
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        let back: Vec<PathBuf> = std::fs::read_dir(base.path()).unwrap().flatten().map(|entry| entry.path()).collect();
        println!(
            "LIVE cut 16 renders, {helpers} processes alive on their profiles when cut, {soon} a tenth of a second after; left: {}, back: {}",
            left.len(),
            back.len()
        );
        // A cut that found no process to kill would have shown nothing.
        assert!(helpers >= 16, "the renders had no Chrome alive when they were cut");
        assert!(left.is_empty(), "left behind: {left:#?}");
        assert!(back.is_empty(), "put back afterwards: {back:?}");
    }

    #[tokio::test]
    #[ignore]
    async fn renders_a_walled_page() {
        // `SCOUT_PROBE_URL` is to be a page that a plain request is refused.
        // What the plain request got is printed first, as `fetch_page`
        // sends it: a render of a page that was never walled says nothing
        // about walls, and it was first run that way (2026-11-22: a shop's
        // front page, which a plain request read in full, while its product
        // pages answered 403 and a challenge).
        let chrome = find_chrome().expect("chrome");
        let url = std::env::var("SCOUT_PROBE_URL").unwrap();
        let client = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap();
        let mut plain = client.get(&url);
        for (name, value) in crate::measure::request_headers() {
            plain = plain.header(name, value);
        }
        let answer = plain.send().await.expect("the plain request");
        let status = answer.status().as_u16();
        let body = answer.text().await.unwrap_or_default();
        println!("LIVE plain status={status} bytes={} challenge={}", body.len(), is_challenge_page(&body));
        // The challenge itself, not any refusal: an address mistyped
        // answers 404, which is no wall either.
        assert!(is_challenge_page(&body), "a plain request to {url} got HTTP {status} and no challenge: it is behind none");

        let html = Renderer::new(chrome).render(&url).await.unwrap();
        let page = crate::tools::fetch::extract_page_for_test(&html, &url);
        println!("LIVE rendered bytes={} challenge={} product={:?}", html.len(), is_challenge_page(&html), page);
        assert!(!is_challenge_page(&html), "the render came back as the challenge");
    }

    #[tokio::test]
    #[ignore]
    async fn cannot_reach_loopback() {
        // A local page that renders fine, asked for by literal and by name.
        // Measured: drop `<-loopback>` and Chrome renders it in full.
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!("<html><body>{}</body></html>", "secret ".repeat(500))),
            )
            .mount(&server)
            .await;
        let chrome = find_chrome().expect("chrome");
        let renderer = Renderer::new(chrome);
        for url in [format!("{}/metrics", server.uri()), format!("http://localhost:{}/", server.address().port())] {
            // A render is refused such an address before its Chrome starts.
            assert!(matches!(renderer.render(&url).await, Err(BrowserError::Refused(_))), "{url}");
            // And a Chrome given it all the same, past that check, still
            // reaches nothing: this is what measures the proxy.
            let html = renderer.render_inner(&url).await.unwrap_or_default();
            assert!(!html.contains("secret"), "{url} rendered: {html}");
        }
        // And the public web still renders through the proxy.
        let html = renderer.render("https://example.com/").await.unwrap();
        assert!(html.contains("Example Domain"), "got: {html}");
    }
}
