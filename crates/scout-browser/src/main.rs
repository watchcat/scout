//! Browser sessions over HTTP, on this machine and for this machine.
//!
//! Nothing calls this yet and nothing deploys it. It listens on loopback
//! and refuses to listen anywhere else: what it serves has no sign-in and
//! no fence around it, and the pod and the network policy that are to be
//! its fence are the card "Chromium in a pod of its own".

use scout_browser::limits::Limits;
use scout_browser::session::Sessions;
use scout_browser::tab::Chrome;
use scout_browser::{chrome, server};
use std::net::SocketAddr;

/// Where it listens when not told: loopback, on a port beside the site's
/// 8080 and the metrics' 9090.
const DEFAULT_BIND: &str = "127.0.0.1:8091";

/// Where to listen: `SCOUT_BROWSER_BIND` if set, and only on loopback.
fn bind_address(asked: Option<String>) -> Result<SocketAddr, String> {
    let asked = asked.filter(|asked| !asked.trim().is_empty()).unwrap_or_else(|| DEFAULT_BIND.to_string());
    let address: SocketAddr = asked.trim().parse().map_err(|_| format!("SCOUT_BROWSER_BIND is not an address and a port: {asked}"))?;
    if !address.ip().is_loopback() {
        return Err(format!("SCOUT_BROWSER_BIND is {address}, and this listens on loopback only: nothing stands between it and whoever can reach it"));
    }
    Ok(address)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let bind = bind_address(std::env::var("SCOUT_BROWSER_BIND").ok())?;
    let chrome = chrome::find_chrome().ok_or("no Chrome was found: set SCOUT_CHROME to one")?;
    tracing::info!(chrome = %chrome.display(), %bind, "browser sessions");
    let sessions = Sessions::new(Chrome::new(chrome), Limits::default());
    let listener = tokio::net::TcpListener::bind(bind).await?;
    // On the way out every session's Chrome goes with the sessions.
    axum::serve(listener, server::router(sessions))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_listens_on_loopback_when_not_told_where() {
        assert_eq!(bind_address(None), Ok("127.0.0.1:8091".parse().unwrap()));
        assert_eq!(bind_address(Some("  ".into())), Ok("127.0.0.1:8091".parse().unwrap()));
        assert_eq!(bind_address(Some("127.0.0.1:9000".into())), Ok("127.0.0.1:9000".parse().unwrap()));
        assert_eq!(bind_address(Some("[::1]:9000".into())), Ok("[::1]:9000".parse().unwrap()));
    }

    #[test]
    fn it_will_not_listen_where_another_machine_could_call() {
        for asked in ["0.0.0.0:8091", "[::]:8091", "192.168.1.10:8091", "10.42.0.7:8091"] {
            let refused = bind_address(Some(asked.into())).unwrap_err();
            assert!(refused.contains("loopback"), "{asked}: {refused}");
        }
        assert!(bind_address(Some("not an address".into())).is_err());
        assert!(bind_address(Some("localhost".into())).is_err(), "an address and a port, not a name");
    }
}
