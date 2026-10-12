//! Keeps requests made on the model's say-so on the public internet.
//!
//! `fetch_page` opens whatever URL the model names, and the model names what
//! the user — or a page Scout has just read — told it to. Without a fence
//! that is a request from inside the pod to anything the pod can reach: its
//! own metrics listener on 0.0.0.0:9090, the site on 8080, every service in
//! the cluster on 10.x, and the cloud metadata service on 169.254.169.254 —
//! with the reply echoed straight into the chat.
//!
//! The fence sits where the connection is made, not where the URL is read,
//! because a URL that passes a check can still end somewhere else: a
//! redirect hop, or a name that resolves to a public address for the check
//! and a private one a moment later (DNS rebinding). So:
//!
//! - the HTTP client resolves names through [`PublicResolver`], which
//!   refuses a name if any address it has is private — the address checked
//!   is the address dialled, so there is no second lookup to rebind;
//! - its redirect policy refuses hops to private IP literals, which never
//!   reach a resolver at all;
//! - [`Guard::check`] answers before anything is sent, so the model gets a
//!   sentence it can act on instead of a connector error;
//! - Chrome is pointed at a SOCKS5 proxy ([`serve_socks`]) that applies the
//!   same rule to every connection it opens, so a redirect, a script that
//!   navigates, or a rebinding name inside the browser meets the same fence.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// What the model is told. Plain, and final: retrying with another spelling
/// of the same address meets the same answer.
pub const NOT_PUBLIC: &str = "that address is not on the public internet — it cannot be opened";

/// Redirect hops followed before giving up; reqwest's own default.
const MAX_REDIRECTS: usize = 10;

/// The destination was refused. A type of its own so a refusal buried in a
/// reqwest error chain can be told apart from a network failure.
#[derive(Debug, thiserror::Error)]
#[error("{NOT_PUBLIC}")]
pub struct NotPublic;

/// Whether `ip` is an address on the public internet.
///
/// Refused: loopback, unspecified and the rest of 0/8, private (10/8,
/// 172.16/12, 192.168/16, fc00::/7, and the retired site-local fec0::/10),
/// link-local (169.254/16, fe80::/10), carrier-grade NAT (100.64/10),
/// broadcast and the reserved 240/4 it sits in, and multicast. An IPv6
/// address that carries an IPv4 one — mapped (::ffff:a.b.c.d), the old
/// compatible form (::a.b.c.d) or NAT64 (64:ff9b::/96) — is judged by the
/// IPv4 address inside it, since that is where the packet ends up.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !(a == 0                                   // this network, incl. unspecified
        || a == 10                             // private
        || a == 127                            // loopback
        || (a == 100 && (64..128).contains(&b)) // carrier-grade NAT
        || (a == 169 && b == 254)              // link-local, cloud metadata
        || (a == 172 && (16..32).contains(&b)) // private
        || (a == 192 && b == 168)              // private
        || a >= 224)                           // multicast, reserved, broadcast
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return false;
    }
    let s = ip.segments();
    let embedded = |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
    // ::ffff:a.b.c.d and ::a.b.c.d (::1 and :: are handled above).
    if s[..5] == [0; 5] && (s[5] == 0xffff || s[5] == 0) {
        return is_public_v4(embedded(s[6], s[7]));
    }
    // NAT64: 64:ff9b::a.b.c.d is translated to a.b.c.d.
    if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return is_public_v4(embedded(s[6], s[7]));
    }
    !((s[0] & 0xfe00) == 0xfc00     // unique local fc00::/7
        || (s[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        || (s[0] & 0xffc0) == 0xfec0 // site-local fec0::/10
        || (s[0] & 0xff00) == 0xff00) // multicast
}

/// Which destinations requests may reach: the public internet, and in tests
/// one exact `ip:port` besides.
///
/// The exemption exists because wiremock listens on 127.0.0.1, and every
/// fetch test would otherwise be refused before it began. It is one socket,
/// not "loopback": a test that redirects from the exempt mock to a second
/// one on the same 127.0.0.1 is refused exactly as production would be.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Guard {
    exempt: Option<SocketAddr>,
}

impl Guard {
    /// Production: the public internet only.
    pub const PUBLIC_ONLY: Guard = Guard { exempt: None };

    /// The public internet plus `addr` — a mock server's socket.
    #[cfg(test)]
    pub fn exempting(addr: SocketAddr) -> Self {
        Self { exempt: Some(addr) }
    }

    fn allows(&self, addr: SocketAddr) -> bool {
        is_public(addr.ip()) || self.exempt == Some(addr)
    }

    /// Every address `host` resolves to, provided all of them are allowed.
    ///
    /// All, not any: a name with one public and one private address would
    /// otherwise reach the private one on whichever attempt the connector
    /// happens to make second.
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await?.collect();
        if addrs.iter().all(|a| self.allows(*a)) {
            Ok(addrs)
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, NotPublic))
        }
    }

    /// Whether `url` may be opened, decided before anything is sent.
    ///
    /// The client would refuse it anyway; this is here so the refusal reads
    /// as [`NOT_PUBLIC`] rather than a connector error, and so the renderer
    /// is never started for a page that cannot be had. A name that does not
    /// resolve passes — the fetch itself will say so in its own words.
    pub async fn check(&self, url: &Url) -> Result<(), NotPublic> {
        let port = url.port_or_known_default().unwrap_or(0);
        match url.host() {
            Some(url::Host::Ipv4(ip)) => self.verdict(SocketAddr::new(ip.into(), port)),
            Some(url::Host::Ipv6(ip)) => self.verdict(SocketAddr::new(ip.into(), port)),
            Some(url::Host::Domain(name)) => match self.resolve(name, port).await {
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(NotPublic),
                _ => Ok(()),
            },
            None => Err(NotPublic),
        }
    }

    fn verdict(&self, addr: SocketAddr) -> Result<(), NotPublic> {
        if self.allows(addr) { Ok(()) } else { Err(NotPublic) }
    }

    /// A client for the open web that cannot be talked into anything else:
    /// names resolve through [`PublicResolver`], and every redirect hop to an
    /// IP literal is put to [`Guard::allows`].
    pub fn client(&self, timeout: Duration) -> reqwest::Result<reqwest::Client> {
        let guard = *self;
        reqwest::Client::builder()
            .timeout(timeout)
            .dns_resolver(Arc::new(PublicResolver))
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= MAX_REDIRECTS {
                    return attempt.error("too many redirects");
                }
                let port = attempt.url().port_or_known_default().unwrap_or(0);
                let literal = match attempt.url().host() {
                    Some(url::Host::Ipv4(ip)) => Some(IpAddr::from(ip)),
                    Some(url::Host::Ipv6(ip)) => Some(IpAddr::from(ip)),
                    _ => None,
                };
                match literal {
                    Some(ip) if !guard.allows(SocketAddr::new(ip, port)) => attempt.error(NotPublic),
                    _ => attempt.follow(),
                }
            }))
            .build()
    }
}

/// The open-web client and the guard it was built with, kept together so the
/// pre-check and the connection can never be asked different questions.
///
/// Separate from the client the API integrations share: those talk to fixed
/// vendor endpoints, and their tests point them at wiremock on 127.0.0.1.
/// This one is for URLs nobody chose but the model.
#[derive(Clone)]
pub struct OpenWeb {
    pub http: reqwest::Client,
    pub guard: Guard,
}

impl OpenWeb {
    pub fn new(timeout: Duration) -> reqwest::Result<Self> {
        Ok(Self { http: Guard::PUBLIC_ONLY.client(timeout)?, guard: Guard::PUBLIC_ONLY })
    }

    /// The open web plus one mock server.
    #[cfg(test)]
    pub fn exempting(addr: SocketAddr) -> Self {
        let guard = Guard::exempting(addr);
        Self { http: guard.client(Duration::from_secs(30)).unwrap(), guard }
    }
}

/// Whether a reqwest error is the guard's refusal rather than the network's.
pub fn refused(err: &reqwest::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = source {
        if e.is::<NotPublic>() {
            return true;
        }
        source = e.source();
    }
    false
}

/// DNS for the open-web client: system resolution, with a name refused
/// outright when any address it has is not public. Never exempts anything —
/// the test exemption is for an IP-literal mock, which no resolver sees.
pub struct PublicResolver;

impl reqwest::dns::Resolve for PublicResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let addrs = Guard::PUBLIC_ONLY.resolve(name.as_str(), 0).await.map_err(|e| {
                // Unwrapped so `refused` finds `NotPublic` in the chain.
                if e.kind() == std::io::ErrorKind::PermissionDenied {
                    Box::new(NotPublic) as Box<dyn std::error::Error + Send + Sync>
                } else {
                    Box::new(e) as Box<dyn std::error::Error + Send + Sync>
                }
            })?;
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// A local SOCKS5 proxy that opens only what `guard` allows, for Chrome.
///
/// SOCKS5 rather than HTTP because Chrome hands it every TCP connection
/// alike — http, https, websockets — and, with `socks5://`, the hostname
/// rather than an address, so the name is resolved here, checked here, and
/// the checked address is the one dialled. Chrome must also be told
/// `--proxy-bypass-list=<-loopback>`, or it goes to localhost directly.
///
/// Returns the port; the proxy, and every tunnel it has open, stops when the
/// returned handle is dropped.
pub async fn serve_socks(guard: Guard) -> std::io::Result<(u16, SocksProxy)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let task = tokio::spawn(async move {
        // Owned by the accept loop, so aborting the loop drops the set and
        // the set aborts every tunnel with it.
        let mut tunnels = tokio::task::JoinSet::new();
        while let Ok((conn, _)) = listener.accept().await {
            tunnels.spawn(async move {
                let _ = socks_session(conn, guard).await;
            });
            // Reap the finished ones so a long render does not hoard them.
            while tunnels.try_join_next().is_some() {}
        }
    });
    Ok((port, SocksProxy(task)))
}

/// Stops the proxy when dropped.
pub struct SocksProxy(tokio::task::JoinHandle<()>);

impl Drop for SocksProxy {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// SOCKS5 reply codes (RFC 1928 §6) this proxy uses.
const REP_OK: u8 = 0x00;
const REP_FAILURE: u8 = 0x01;
const REP_NOT_ALLOWED: u8 = 0x02;
const REP_UNREACHABLE: u8 = 0x04;
const REP_COMMAND: u8 = 0x07;
const REP_ADDRESS_TYPE: u8 = 0x08;

/// One client: no-auth handshake, a single CONNECT, then a byte pipe.
async fn socks_session(mut conn: TcpStream, guard: Guard) -> std::io::Result<()> {
    // Greeting: VER, NMETHODS, METHODS. Only "no authentication" is offered.
    let mut head = [0u8; 2];
    conn.read_exact(&mut head).await?;
    let mut methods = vec![0u8; head[1] as usize];
    conn.read_exact(&mut methods).await?;
    if head[0] != 5 || !methods.contains(&0) {
        conn.write_all(&[5, 0xff]).await?;
        return Ok(());
    }
    conn.write_all(&[5, 0]).await?;

    // Request: VER, CMD, RSV, ATYP, DST.ADDR, DST.PORT.
    let mut req = [0u8; 4];
    conn.read_exact(&mut req).await?;
    if req[1] != 1 {
        return reply(&mut conn, REP_COMMAND).await;
    }
    let target = match req[3] {
        1 => {
            let mut ip = [0u8; 4];
            conn.read_exact(&mut ip).await?;
            Target::Ip(IpAddr::from(ip))
        }
        4 => {
            let mut ip = [0u8; 16];
            conn.read_exact(&mut ip).await?;
            Target::Ip(IpAddr::from(ip))
        }
        3 => {
            let len = conn.read_u8().await? as usize;
            let mut name = vec![0u8; len];
            conn.read_exact(&mut name).await?;
            Target::Name(String::from_utf8_lossy(&name).into_owned())
        }
        _ => return reply(&mut conn, REP_ADDRESS_TYPE).await,
    };
    let port = conn.read_u16().await?;

    let addrs = match target {
        Target::Ip(ip) => vec![SocketAddr::new(ip, port)],
        // Bracketed IPv6 or a dotted quad sent as a "name" is still a
        // literal, and must not skip the check by arriving as text.
        Target::Name(name) => match name.trim_matches(['[', ']']).parse::<IpAddr>() {
            Ok(ip) => vec![SocketAddr::new(ip, port)],
            Err(_) => match guard.resolve(&name, port).await {
                Ok(addrs) => addrs,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    tracing::warn!(host = %name, "renderer refused a non-public address");
                    return reply(&mut conn, REP_NOT_ALLOWED).await;
                }
                Err(_) => return reply(&mut conn, REP_UNREACHABLE).await,
            },
        },
    };
    if !addrs.iter().all(|a| guard.allows(*a)) {
        tracing::warn!(addr = ?addrs, "renderer refused a non-public address");
        return reply(&mut conn, REP_NOT_ALLOWED).await;
    }
    let mut upstream = match TcpStream::connect(&addrs[..]).await {
        Ok(s) => s,
        Err(_) => return reply(&mut conn, REP_FAILURE).await,
    };
    reply(&mut conn, REP_OK).await?;
    tokio::io::copy_bidirectional(&mut conn, &mut upstream).await?;
    Ok(())
}

enum Target {
    Ip(IpAddr),
    Name(String),
}

/// A reply with an all-zero bound address: Chrome does not use it.
async fn reply(conn: &mut TcpStream, code: u8) -> std::io::Result<()> {
    conn.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn every_non_public_range_is_refused() {
        for addr in [
            // loopback, the whole /8 and not just .1
            "127.0.0.1", "127.255.255.254", "::1",
            // unspecified, and the rest of "this network"
            "0.0.0.0", "0.1.2.3", "::",
            // private
            "10.0.0.1", "10.43.0.10", "172.16.0.1", "172.31.255.255", "192.168.1.1",
            "fc00::1", "fd12:3456::1", "fec0::1",
            // link-local; 169.254.169.254 is the metadata service
            "169.254.169.254", "169.254.0.1", "fe80::1",
            // carrier-grade NAT
            "100.64.0.1", "100.127.255.255",
            // broadcast, reserved, multicast
            "255.255.255.255", "240.0.0.1", "224.0.0.1", "239.255.255.250", "ff02::1",
            // IPv4 inside IPv6: mapped, compatible, NAT64
            "::ffff:127.0.0.1", "::ffff:169.254.169.254", "::ffff:10.0.0.1",
            "::127.0.0.1", "64:ff9b::a00:1",
        ] {
            assert!(!is_public(ip(addr)), "{addr} must be refused");
        }
    }

    #[test]
    fn public_addresses_pass_including_the_neighbours_of_each_range() {
        for addr in [
            "1.1.1.1", "8.8.8.8", "93.184.216.34",
            // one step outside a refused range
            "11.0.0.1", "126.255.255.255", "128.0.0.1", "172.15.255.255", "172.32.0.1",
            "192.167.255.255", "192.169.0.1", "169.253.0.1", "100.63.255.255", "100.128.0.1",
            "223.255.255.255",
            "2606:4700::1111", "2a01:4f8::1", "::ffff:1.1.1.1", "64:ff9b::808:808",
        ] {
            assert!(is_public(ip(addr)), "{addr} must pass");
        }
    }

    #[tokio::test]
    async fn check_refuses_literals_in_every_spelling_and_names_that_resolve_inside() {
        let g = Guard::PUBLIC_ONLY;
        for url in [
            "http://127.0.0.1:9090/metrics",
            "http://127.1:8080/",
            "http://2130706433/",          // 127.0.0.1 as one number
            "http://0x7f000001/",
            "http://[::1]:9090/",
            "http://[::ffff:169.254.169.254]/",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.43.0.1/",
            "http://localhost:9090/metrics",
        ] {
            assert!(g.check(&Url::parse(url).unwrap()).await.is_err(), "{url} must be refused");
        }
        assert!(g.check(&Url::parse("http://1.1.1.1/").unwrap()).await.is_ok());
        // A name that does not resolve is the fetch's to report, not ours.
        assert!(g.check(&Url::parse("https://nx.invalid/").unwrap()).await.is_ok());
    }

    #[tokio::test]
    async fn the_exemption_is_one_socket_not_loopback() {
        let a: SocketAddr = "127.0.0.1:4000".parse().unwrap();
        let g = Guard::exempting(a);
        assert!(g.check(&Url::parse("http://127.0.0.1:4000/x").unwrap()).await.is_ok());
        assert!(g.check(&Url::parse("http://127.0.0.1:4001/x").unwrap()).await.is_err());
        assert!(g.check(&Url::parse("http://127.0.0.2:4000/x").unwrap()).await.is_err());
    }

    #[tokio::test]
    async fn the_client_refuses_a_redirect_to_loopback_and_never_reaches_it() {
        let public = MockServer::start().await;
        let private = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/metrics"))
            .respond_with(ResponseTemplate::new(200).set_body_string("secret_total 1"))
            .expect(0)
            .mount(&private)
            .await;
        Mock::given(method("GET"))
            .and(path("/go"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/metrics", private.uri())),
            )
            .mount(&public)
            .await;

        let http = Guard::exempting(*public.address()).client(Duration::from_secs(5)).unwrap();
        let err = http.get(format!("{}/go", public.uri())).send().await.unwrap_err();
        assert!(refused(&err), "expected the guard's refusal, got: {err:?}");
    }

    #[tokio::test]
    async fn the_client_refuses_a_redirect_to_the_metadata_service() {
        let public = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(301)
                    .insert_header("Location", "http://169.254.169.254/latest/meta-data/"),
            )
            .mount(&public)
            .await;
        let http = Guard::exempting(*public.address()).client(Duration::from_secs(5)).unwrap();
        let err = http.get(public.uri()).send().await.unwrap_err();
        assert!(refused(&err), "got: {err:?}");
    }

    #[tokio::test]
    async fn the_client_refuses_a_name_that_resolves_to_loopback() {
        // A name, so it goes through the resolver — the path a rebinding
        // domain would take. "localhost" is the one name every box has.
        let server = MockServer::start().await;
        let port = server.address().port();
        let http = Guard::exempting(*server.address()).client(Duration::from_secs(5)).unwrap();
        let err = http.get(format!("http://localhost:{port}/")).send().await.unwrap_err();
        assert!(refused(&err), "got: {err:?}");
    }

    /// Drives one SOCKS5 CONNECT and returns the reply code.
    async fn socks_connect(proxy: u16, atyp: u8, addr: &[u8], port: u16) -> (u8, TcpStream) {
        let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy)).await.unwrap();
        s.write_all(&[5, 1, 0]).await.unwrap();
        let mut hello = [0u8; 2];
        s.read_exact(&mut hello).await.unwrap();
        assert_eq!(hello, [5, 0]);
        let mut req = vec![5, 1, 0, atyp];
        if atyp == 3 {
            req.push(addr.len() as u8);
        }
        req.extend_from_slice(addr);
        req.extend_from_slice(&port.to_be_bytes());
        s.write_all(&req).await.unwrap();
        let mut rep = [0u8; 10];
        s.read_exact(&mut rep).await.unwrap();
        (rep[1], s)
    }

    #[tokio::test]
    async fn the_renderer_proxy_refuses_private_addresses_however_they_are_sent() {
        let (proxy, _keep) = serve_socks(Guard::PUBLIC_ONLY).await.unwrap();
        for (atyp, addr) in [
            (1, &[127, 0, 0, 1][..]),
            (1, &[169, 254, 169, 254][..]),
            (1, &[10, 0, 0, 1][..]),
            (4, &Ipv6Addr::LOCALHOST.octets()[..]),
            (3, &b"localhost"[..]),
            (3, &b"127.0.0.1"[..]),
            (3, &b"[::1]"[..]),
        ] {
            let (code, _) = socks_connect(proxy, atyp, addr, 9090).await;
            assert_eq!(code, REP_NOT_ALLOWED, "atyp {atyp} {addr:?}");
        }
    }

    #[tokio::test]
    async fn the_renderer_proxy_tunnels_to_an_allowed_address() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("hello"))
            .mount(&server)
            .await;
        let addr = *server.address();
        let (proxy, _keep) = serve_socks(Guard::exempting(addr)).await.unwrap();
        let (code, mut s) = socks_connect(proxy, 1, &[127, 0, 0, 1], addr.port()).await;
        assert_eq!(code, REP_OK);
        s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await.unwrap();
        let mut body = String::new();
        s.read_to_string(&mut body).await.unwrap();
        assert!(body.ends_with("hello"), "got: {body}");
    }
}
