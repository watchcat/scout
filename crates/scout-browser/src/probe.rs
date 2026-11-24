//! `scout-browser probe …`: what this pod can reach, asked from inside it.
//!
//! `scripts/check-browser-fence.sh` runs a probe in the browser pod and,
//! as its control, in a pod it starts from the same image that no policy
//! holds, so that a probe that must fail counts only where the same probe
//! could have succeeded. A fence proved by a probe that can never succeed
//! is proved by nothing. From Scout's pod it asks for a render.
//! The image has no curl and no nc, and the question is about the pod's
//! network, not the node's, so the binary asks it itself.
//!
//! ```text
//! scout-browser probe tcp <host:port>   a TCP connection, the name resolved here
//! scout-browser probe dns <name>        a name, resolved by this pod's resolver
//! scout-browser probe udp <ip:port>     a datagram out: a DNS question, and its answer
//! scout-browser probe render <url>      a render asked of SCOUT_BROWSER_URL, as Scout asks one
//! ```
//!
//! Each prints one line, `OK …` or `FAIL …`, and exits 0 or 1.

use std::net::SocketAddr;
use std::time::Duration;

/// How long a probe waits for an answer. A render has its own deadline,
/// the one Scout's renders have.
pub const WITHIN: Duration = Duration::from_secs(5);

/// What a probe asks.
#[derive(Debug, PartialEq)]
pub enum Probe {
    Tcp(String),
    Dns(String),
    Udp(SocketAddr),
    Render(String),
}

const USAGE: &str = "usage: scout-browser probe tcp <host:port> | dns <name> | udp <ip:port> | render <url>";

/// A DNS question for `example.com`'s address, as a resolver is asked.
/// Any server for names answers it with something, even a refusal, so
/// an answer is the datagram's way there and back.
const QUESTION: [u8; 29] = [
    0x5c, 0x07, // the question's number, which the answer repeats
    0x01, 0x00, // a standard query, recursion wanted
    0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // one question, nothing else
    7, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0, //
    0x00, 0x01, 0x00, 0x01, // an address, on the internet
];

/// The probe `args` (what follows `probe`) asks for.
pub fn parse(args: &[String]) -> Result<Probe, String> {
    let [kind, target] = args else { return Err(USAGE.to_string()) };
    match kind.as_str() {
        "tcp" => Ok(Probe::Tcp(target.clone())),
        "dns" => Ok(Probe::Dns(target.clone())),
        "udp" => target.parse().map(Probe::Udp).map_err(|_| format!("udp is sent to an address and a port, not {target}")),
        "render" => Ok(Probe::Render(target.clone())),
        _ => Err(USAGE.to_string()),
    }
}

/// The line to print for `args`, and whether what was asked was reached.
pub async fn answer(args: &[String]) -> (String, bool) {
    let probe = match parse(args) {
        Ok(probe) => probe,
        Err(usage) => return (format!("FAIL {usage}"), false),
    };
    let pod = std::env::var("SCOUT_BROWSER_URL").ok();
    match run(probe, WITHIN, pod.as_deref()).await {
        Ok(reached) => (format!("OK {reached}"), true),
        Err(why) => (format!("FAIL {why}"), false),
    }
}

/// One probe: what was reached, or why not. `pod` is the browser pod's
/// address, for a render.
pub async fn run(probe: Probe, within: Duration, pod: Option<&str>) -> Result<String, String> {
    let no_answer = || format!("no answer within {} seconds", within.as_secs_f32());
    match probe {
        Probe::Tcp(target) => tokio::time::timeout(within, tcp(&target)).await.map_err(|_| no_answer())?,
        Probe::Dns(name) => tokio::time::timeout(within, dns(&name)).await.map_err(|_| no_answer())?,
        Probe::Udp(to) => tokio::time::timeout(within, udp(to)).await.map_err(|_| no_answer())?,
        Probe::Render(url) => render(&url, pod).await,
    }
}

async fn tcp(target: &str) -> Result<String, String> {
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host(target)
        .await
        .map_err(|e| format!("{target} is not a reachable name and port: {e}"))?
        .collect();
    // A share of the probe's time each, so that a first address that
    // swallows the connection does not leave none for the next.
    let each = WITHIN / u32::try_from(addresses.len().max(1)).unwrap_or(u32::MAX);
    let mut failed = format!("{target} resolved to nothing");
    for address in addresses {
        match tokio::time::timeout(each, tokio::net::TcpStream::connect(address)).await {
            Ok(Ok(_)) => return Ok(format!("connected to {target}")),
            Ok(Err(e)) => failed = format!("{target}: {e}"),
            Err(_) => failed = format!("{target}: no answer from one of its addresses"),
        }
    }
    Err(failed)
}

async fn dns(name: &str) -> Result<String, String> {
    let found = tokio::net::lookup_host((name, 0)).await.map_err(|e| format!("{name} did not resolve: {e}"))?.count();
    Ok(format!("{name} resolved to {found} address{}", if found == 1 { "" } else { "es" }))
}

async fn udp(to: SocketAddr) -> Result<String, String> {
    let from = if to.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let socket = tokio::net::UdpSocket::bind(from).await.map_err(|e| format!("no socket to send from: {e}"))?;
    socket.send_to(&QUESTION, to).await.map_err(|e| format!("could not send to {to}: {e}"))?;
    let mut answer = [0u8; 1500];
    loop {
        let (length, sender) = socket.recv_from(&mut answer).await.map_err(|e| format!("nothing came back from {to}: {e}"))?;
        if sender == to && length >= 2 && answer[..2] == QUESTION[..2] {
            return Ok(format!("{to} answered a datagram"));
        }
    }
}

async fn render(url: &str, pod: Option<&str>) -> Result<String, String> {
    let pod = pod.map(str::trim).filter(|pod| !pod.is_empty()).ok_or("SCOUT_BROWSER_URL is not set, and a render is asked of the pod it names")?;
    let pod = url::Url::parse(pod).map_err(|e| format!("SCOUT_BROWSER_URL is not an address: {e}"))?;
    let renderer = scout_core::render::Renderer::remote(&pod).map_err(|e| e.to_string())?;
    let page = renderer.render(url).await.map_err(|e| e.to_string())?;
    Ok(format!("rendered {} bytes, titled {:?}", page.len(), title_of(&page)))
}

/// What a page's `<title>` says, roughly: enough to tell one page from another.
fn title_of(page: &str) -> String {
    let lower = page.to_ascii_lowercase();
    let Some(open) = lower.find("<title") else { return String::new() };
    let Some(start) = lower[open..].find('>').map(|end| open + end + 1) else { return String::new() };
    let end = lower[start..].find("</title>").map_or(page.len(), |end| start + end);
    page.get(start..end).unwrap_or_default().trim().chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asked(args: &[&str]) -> Result<Probe, String> {
        parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn a_probe_is_one_of_four_and_its_one_target() {
        assert_eq!(asked(&["tcp", "example.com:443"]), Ok(Probe::Tcp("example.com:443".into())));
        assert_eq!(asked(&["dns", "example.com"]), Ok(Probe::Dns("example.com".into())));
        assert_eq!(asked(&["udp", "1.1.1.1:53"]), Ok(Probe::Udp("1.1.1.1:53".parse().unwrap())));
        assert_eq!(asked(&["render", "https://example.com/"]), Ok(Probe::Render("https://example.com/".into())));
        for wrong in [&[][..], &["tcp"], &["tcp", "a:1", "b:2"], &["http", "example.com"]] {
            assert_eq!(asked(wrong), Err(USAGE.to_string()), "{wrong:?}");
        }
        assert!(asked(&["udp", "one.one.one.one:53"]).unwrap_err().contains("an address and a port"));
    }

    #[tokio::test]
    async fn tcp_reaches_a_listener_and_not_a_closed_port() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = listener.local_addr().unwrap().to_string();
        let reached = run(Probe::Tcp(open.clone()), WITHIN, None).await;
        assert_eq!(reached, Ok(format!("connected to {open}")));
        drop(listener);
        let refused = run(Probe::Tcp(open.clone()), WITHIN, None).await.unwrap_err();
        assert!(refused.starts_with(&open), "{refused}");
    }

    #[tokio::test]
    async fn dns_resolves_a_name_and_says_when_it_does_not() {
        assert!(run(Probe::Dns("localhost".into()), WITHIN, None).await.unwrap().starts_with("localhost resolved to "));
        assert!(run(Probe::Dns("nowhere.invalid".into()), WITHIN, None).await.is_err());
    }

    #[tokio::test]
    async fn udp_is_reached_when_its_question_is_answered_and_not_when_nothing_comes_back() {
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let at = server.local_addr().unwrap();
        let answering = tokio::spawn(async move {
            let mut asked = [0u8; 512];
            let (length, from) = server.recv_from(&mut asked).await.unwrap();
            assert_eq!(&asked[..length], &QUESTION[..], "the question is a DNS question");
            // A first datagram that is not the answer, then the answer.
            server.send_to(b"\x00\x00noise", from).await.unwrap();
            server.send_to(&asked[..length], from).await.unwrap();
        });
        assert_eq!(run(Probe::Udp(at), WITHIN, None).await, Ok(format!("{at} answered a datagram")));
        answering.await.unwrap();

        let silent = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let at = silent.local_addr().unwrap();
        let unanswered = run(Probe::Udp(at), Duration::from_millis(300), None).await.unwrap_err();
        assert_eq!(unanswered, "no answer within 0.3 seconds");
        drop(silent);
    }

    #[tokio::test]
    async fn udp_is_not_reached_by_another_question_or_by_an_answer_from_elsewhere() {
        // Where the fence drops the datagram, nothing may stand in for the
        // answer: not another number from the server asked, and not the
        // right number from another address.
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let elsewhere = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let at = server.local_addr().unwrap();
        let answering = tokio::spawn(async move {
            let mut asked = [0u8; 512];
            let (length, from) = server.recv_from(&mut asked).await.unwrap();
            let mut other = asked[..length].to_vec();
            other[0] ^= 0xff;
            server.send_to(&other, from).await.unwrap();
            elsewhere.send_to(&asked[..length], from).await.unwrap();
            (server, elsewhere)
        });
        let unanswered = run(Probe::Udp(at), Duration::from_millis(500), None).await.unwrap_err();
        assert_eq!(unanswered, "no answer within 0.5 seconds");
        drop(answering.await.unwrap());
    }

    #[tokio::test]
    async fn a_render_is_asked_of_the_pod_and_only_with_one_named() {
        assert!(run(Probe::Render("https://example.com/".into()), WITHIN, None).await.unwrap_err().contains("SCOUT_BROWSER_URL is not set"));
        assert!(run(Probe::Render("https://example.com/".into()), WITHIN, Some("  ")).await.unwrap_err().contains("SCOUT_BROWSER_URL is not set"));
        // A pod with nothing listening: the render fails, and says so.
        let gone = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        let failed = run(Probe::Render("https://example.com/".into()), WITHIN, Some(&format!("http://{gone}"))).await.unwrap_err();
        assert!(failed.contains("the browser pod could not be asked"), "{failed}");
    }

    #[test]
    fn a_pages_title_is_read_roughly() {
        assert_eq!(title_of("<html><head><TITLE>Example Domain</TITLE></head>"), "Example Domain");
        assert_eq!(title_of("<title lang=\"nl\"> Widget kopen </title>"), "Widget kopen");
        assert_eq!(title_of("<html>no title</html>"), "");
        assert_eq!(title_of("<title>never closed"), "never closed");
    }
}
