//! The browser pod's server: renders for Scout, and browser sessions.
//!
//! In production it runs in a pod of its own (`deploy/k8s/browser.yaml`),
//! with nothing of Scout's in it, and a network policy lets only Scout's
//! pod in and only the public internet out. Anywhere else it listens on
//! loopback and refuses to listen beyond it unless told what fences it.
//!
//! `scout-browser probe …` asks what the pod can reach, from inside it:
//! `scripts/check-browser-fence.sh` runs it there (`probe.rs`).

use scout_browser::limits::Limits;
use scout_browser::session::Sessions;
use scout_browser::tab::Chrome;
use scout_browser::{chrome, probe, server};
use std::net::SocketAddr;
use std::sync::Arc;

/// Where it listens when not told: loopback, on a port beside the site's
/// 8080 and the metrics' 9090.
const DEFAULT_BIND: &str = "127.0.0.1:8091";

/// Where to listen: `SCOUT_BROWSER_BIND` if set, on loopback unless
/// `SCOUT_BROWSER_FENCE` says what stands between it and the rest of the
/// network. Nothing here has a sign-in, so beyond loopback the fence is all
/// there is. In the pod that is its network policy, which the pod's
/// manifest names; on a laptop, a bind address set by mistake is still
/// refused. Answers with the fence it was told of, to be logged.
fn bind_address(asked: Option<String>, fence: Option<String>) -> Result<(SocketAddr, Option<String>), String> {
    let asked = asked.filter(|asked| !asked.trim().is_empty()).unwrap_or_else(|| DEFAULT_BIND.to_string());
    let address: SocketAddr = asked.trim().parse().map_err(|_| format!("SCOUT_BROWSER_BIND is not an address and a port: {asked}"))?;
    let fence = fence.map(|fence| fence.trim().to_string()).filter(|fence| !fence.is_empty());
    if !address.ip().is_loopback() && fence.is_none() {
        return Err(format!(
            "SCOUT_BROWSER_BIND is {address}, and this listens beyond loopback only when SCOUT_BROWSER_FENCE says what fences it: \
             nothing here has a sign-in"
        ));
    }
    Ok((address, fence))
}

/// A `SIGTERM`, which is how a pod is stopped, or a Ctrl-C. The server then
/// stops taking calls and lets those under way finish.
async fn stopped() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(e) => {
                tracing::warn!("no SIGTERM handler, so only a Ctrl-C stops this gracefully: {e}");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("probe") {
        let (said, reached) = probe::answer(&args[1..]).await;
        println!("{said}");
        std::process::exit(if reached { 0 } else { 1 });
    }

    let (bind, fence) = bind_address(std::env::var("SCOUT_BROWSER_BIND").ok(), std::env::var("SCOUT_BROWSER_FENCE").ok())?;
    let chrome = chrome::find_chrome().ok_or("no Chrome was found: set SCOUT_CHROME to one")?;
    let renders = scout_core::render::find_chrome().map(|chrome| Arc::new(scout_core::render::Renderer::new(chrome)));
    tracing::info!(
        chrome = %chrome.display(),
        renders = renders.as_ref().map(|r| r.describe()).unwrap_or_else(|| "nothing: no Chrome was found".to_string()),
        %bind,
        fence = fence.as_deref().unwrap_or("loopback"),
        "browser"
    );
    let sessions = Sessions::new(Chrome::new(chrome), Limits::default());
    let listener = tokio::net::TcpListener::bind(bind).await?;
    // On the way out every session's Chrome goes with the sessions, and a
    // render's with its render.
    axum::serve(listener, server::router(sessions, renders)).with_graceful_shutdown(stopped()).await?;
    tracing::info!("stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bound(asked: Option<&str>, fence: Option<&str>) -> Result<SocketAddr, String> {
        bind_address(asked.map(str::to_string), fence.map(str::to_string)).map(|(address, _)| address)
    }

    #[test]
    fn it_listens_on_loopback_when_not_told_where() {
        assert_eq!(bound(None, None), Ok("127.0.0.1:8091".parse().unwrap()));
        assert_eq!(bound(Some("  "), None), Ok("127.0.0.1:8091".parse().unwrap()));
        assert_eq!(bound(Some("127.0.0.1:9000"), None), Ok("127.0.0.1:9000".parse().unwrap()));
        assert_eq!(bound(Some("[::1]:9000"), None), Ok("[::1]:9000".parse().unwrap()));
    }

    #[test]
    fn it_will_not_listen_where_another_machine_could_call_unless_told_what_fences_it() {
        for asked in ["0.0.0.0:8091", "[::]:8091", "192.168.1.10:8091", "10.42.0.7:8091"] {
            for fence in [None, Some(""), Some("   ")] {
                let refused = bound(Some(asked), fence).unwrap_err();
                assert!(refused.contains("loopback") && refused.contains("SCOUT_BROWSER_FENCE"), "{asked} {fence:?}: {refused}");
            }
        }
        assert!(bound(Some("not an address"), Some("a policy")).is_err());
        assert!(bound(Some("localhost"), None).is_err(), "an address and a port, not a name");
    }

    #[test]
    fn it_listens_beyond_loopback_when_told_what_fences_it() {
        let told = bind_address(Some("0.0.0.0:8091".into()), Some(" the scout-browser NetworkPolicy ".into()));
        assert_eq!(told, Ok(("0.0.0.0:8091".parse().unwrap(), Some("the scout-browser NetworkPolicy".into()))));
        // Told of a fence, loopback is still loopback.
        assert_eq!(bound(None, Some("a policy")), Ok("127.0.0.1:8091".parse().unwrap()));
    }
}
