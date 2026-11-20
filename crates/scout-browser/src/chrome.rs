//! The Chrome a session runs in: where it is, and how it is started.
//!
//! One Chrome a session, with a profile of its own that goes when the
//! session does, as each render has in `scout-core`'s `tools/browser.rs`.
//! Every connection it makes goes through `scout_core::net_guard`'s proxy,
//! so a redirect, a script or a name that resolves inwards meets the fence
//! the render meets.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Where Chrome lives, if it does: `SCOUT_CHROME`, else the places a
/// package puts it. The same search as `find_chrome` in `scout-core`'s
/// `tools/browser.rs`, which is private with the rest of `tools`; a path
/// set and not there finds nothing, as there.
pub fn find_chrome() -> Option<PathBuf> {
    chrome_from(std::env::var("SCOUT_CHROME").ok())
}

fn chrome_from(named: Option<String>) -> Option<PathBuf> {
    if let Some(path) = named {
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

/// Chrome's command line for one session behind the guard's proxy on
/// `proxy`. `reaching` is one socket Chrome may dial directly, for a test's
/// own page server on loopback; a session outside a test has none.
pub(crate) fn chrome_args(profile: &Path, proxy: u16, reaching: Option<SocketAddr>) -> Vec<String> {
    // What the render sends and nothing more: no flag here hides that the
    // browser is driven. A shop that refuses a driven browser is a shop a
    // session does not browse.
    let agent = scout_core::measure::request_headers()
        .iter()
        .find(|(name, _)| *name == "User-Agent")
        .map(|(_, agent)| *agent)
        .unwrap_or_default();
    // Chrome sends localhost and loopback around any proxy unless told not
    // to, which would walk past the fence. A later rule can put one socket
    // back, and only that socket: measured 2026-11-20 on Chrome 157, with
    // `<-loopback>;127.0.0.1:A` a page on port A loaded directly while
    // 127.0.0.1 on another port, and port A asked for as `localhost`, both
    // went to the proxy.
    let bypass = match reaching {
        Some(socket) => format!("--proxy-bypass-list=<-loopback>;{socket}"),
        None => "--proxy-bypass-list=<-loopback>".to_string(),
    };
    vec![
        "--headless=new".into(),
        format!("--user-data-dir={}", profile.display()),
        format!("--user-agent={agent}"),
        // socks5, so that Chrome hands the proxy the name and the guard
        // resolves, checks and dials it in one place.
        format!("--proxy-server=socks5://127.0.0.1:{proxy}"),
        bypass,
        // Headless Chrome 157 answered a notification and a location
        // request with a refusal without this (measured 2026-11-20); the
        // flag is here so that the answer does not rest on the mode.
        "--deny-permission-prompts".into(),
        "--disable-gpu".into(),
        "--disable-extensions".into(),
        "--no-first-run".into(),
        "--window-size=1280,1024".into(),
        "--lang=nl-NL".into(),
        "--mute-audio".into(),
        "about:blank".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_goes_through_the_guard_proxy_loopback_included() {
        let args = chrome_args(Path::new("/tmp/p"), 4321, None);
        assert!(args.contains(&"--proxy-server=socks5://127.0.0.1:4321".to_string()));
        // Without this Chrome reaches 127.0.0.1 directly, proxy or not.
        assert!(args.contains(&"--proxy-bypass-list=<-loopback>".to_string()));
        assert!(args.contains(&"--user-data-dir=/tmp/p".to_string()));
        assert!(!args.iter().any(|arg| arg.starts_with("--remote-debugging-port")), "the protocol is on the pipe, never a port");
    }

    #[test]
    fn a_tests_page_server_is_one_socket_past_the_proxy_and_not_loopback() {
        let server: SocketAddr = "127.0.0.1:4000".parse().unwrap();
        let args = chrome_args(Path::new("/tmp/p"), 4321, Some(server));
        // The loopback rule first, then the one socket: every other port of
        // 127.0.0.1, and the same port by name, still goes to the guard.
        assert!(args.contains(&"--proxy-bypass-list=<-loopback>;127.0.0.1:4000".to_string()), "{args:?}");
        assert!(args.contains(&"--proxy-server=socks5://127.0.0.1:4321".to_string()));
    }

    #[test]
    fn a_session_says_it_is_the_browser_the_render_says_it_is_and_no_more() {
        let args = chrome_args(Path::new("/tmp/p"), 4321, None);
        let agent = scout_core::measure::request_headers().iter().find(|(name, _)| *name == "User-Agent").unwrap().1;
        assert!(args.contains(&format!("--user-agent={agent}")));
        assert!(args.contains(&"--headless=new".to_string()));
        // Nothing that hides what drives it.
        assert!(!args.iter().any(|arg| arg.contains("AutomationControlled") || arg.contains("enable-automation")));
    }

    #[test]
    fn permission_prompts_are_answered_no() {
        assert!(chrome_args(Path::new("/tmp/p"), 4321, None).contains(&"--deny-permission-prompts".to_string()));
    }

    #[test]
    fn a_chrome_named_and_not_there_is_no_chrome() {
        // As in scout-core: a path that does not exist must not fall back
        // to another Chrome than the one that was named.
        assert_eq!(chrome_from(Some("/definitely/not/here".to_string())), None);
        assert_eq!(chrome_from(Some("/bin/sh".to_string())), Some(PathBuf::from("/bin/sh")));
    }
}
