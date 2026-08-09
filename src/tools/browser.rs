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

/// Renders `url` and returns the DOM after scripts have run.
///
/// Cloned into every `fetch_page` tool, so the slots are shared: the limit is
/// on Chromes alive in this process, not on renders per request.
#[derive(Clone)]
pub struct Renderer {
    chrome: PathBuf,
    slots: Arc<Semaphore>,
}

impl Renderer {
    pub fn new(chrome: PathBuf) -> Self {
        Self { chrome, slots: Arc::new(Semaphore::new(MAX_CONCURRENT_RENDERS)) }
    }

    pub async fn render(&self, url: &str) -> Result<String, BrowserError> {
        let started = Instant::now();
        // Queueing is part of the deadline rather than added to it, so a
        // busy renderer cannot turn a 45-second ceiling into ninety.
        let _slot = match tokio::time::timeout(RENDER_TIMEOUT, self.slots.acquire()).await {
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

    /// One-shot `--dump-dom`, deliberately not a CDP session.
    ///
    /// Driving Chrome over CDP is the obvious approach and it does not work
    /// here: an automated session announces itself (`navigator.webdriver`
    /// among others) and action.com's challenge never clears — measured at
    /// 67 seconds of real time, still the interstitial. The same Chrome as a
    /// plain subprocess passes it every time.
    async fn render_inner(&self, url: &str) -> Result<String, BrowserError> {
        // Each render gets a throwaway profile: concurrent runs sharing one
        // would fight over its lock.
        let profile = std::env::temp_dir().join(format!(
            "scout-chrome-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));

        let mut child = tokio::process::Command::new(&self.chrome)
            .arg("--headless=new")
            .arg("--dump-dom")
            .arg(format!("--virtual-time-budget={VIRTUAL_TIME_BUDGET_MS}"))
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg(format!("--user-agent={}", super::fetch::BROWSER_USER_AGENT))
            // --no-sandbox is required to run as a non-root user in a
            // container; --disable-dev-shm-usage keeps Chrome from dying on
            // the small /dev/shm a container gets.
            .arg("--no-sandbox")
            .arg("--disable-dev-shm-usage")
            .arg("--disable-gpu")
            .arg("--disable-extensions")
            .arg("--no-first-run")
            .arg("--window-size=1280,1024")
            .arg("--lang=nl-NL")
            .arg("--mute-audio")
            .arg(url)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| BrowserError::Launch(e.to_string()))?;

        // Chrome prints the DOM and then lingers — waiting for it to exit
        // means waiting forever (measured: the full page in stdout, the
        // process still alive two minutes later). So read the pipe instead,
        // and stop once it goes quiet.
        let mut stdout = child.stdout.take().expect("stdout is piped");
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
        let _ = child.start_kill();
        let _ = child.wait().await;
        let _ = std::fs::remove_dir_all(&profile);

        if html.is_empty() {
            return Err(BrowserError::Timeout);
        }
        Ok(String::from_utf8_lossy(&html).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn renders_a_walled_page() {
        let chrome = find_chrome().expect("chrome");
        let url = std::env::var("SCOUT_PROBE_URL").unwrap();
        let html = Renderer::new(chrome).render(&url).await.unwrap();
        let page = crate::tools::fetch::extract_page_for_test(&html, &url);
        println!("LIVE bytes={} product={:?}", html.len(), page);
    }
}
