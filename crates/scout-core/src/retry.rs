//! One more try, for the providers Scout pays.
//!
//! Every paid API — search, flights, marketplaces — got exactly one attempt,
//! so a single 429 or a gateway hiccup was a failed tool call, and the model
//! then either gave up on that source or spent a turn asking again. This
//! sends the same request again, a little later, when the failure was the
//! provider's rather than ours.
//!
//! What counts as theirs is deliberately narrow: a 429 or a 5xx, and a
//! connection that was refused. A timeout is not retried, because a request
//! that timed out may have been executed and billed — Duffel charges per
//! search — and a second one would be a second charge for a search that
//! already ran. A 4xx other than 429 is the caller's mistake and will not
//! get better by repetition.
//!
//! The caller's own status check still runs on whatever comes back last, so
//! each provider keeps wording its own errors exactly as it did.

use std::time::Duration;

/// Attempts in total, the first included.
pub(crate) const ATTEMPTS: usize = 3;

/// The wait before each retry. Short, because a run has a stall budget of
/// ninety seconds and a tool call has to fit inside it with room to spare.
#[cfg(not(test))]
pub(crate) const DELAYS: [Duration; ATTEMPTS - 1] =
    [Duration::from_millis(500), Duration::from_millis(2000)];
/// Every provider test that mounts a 5xx would otherwise wait the real
/// two and a half seconds to learn what it already knows.
#[cfg(test)]
pub(crate) const DELAYS: [Duration; ATTEMPTS - 1] = [Duration::from_millis(1), Duration::from_millis(2)];

/// The most a `Retry-After` header is allowed to ask for. Above this the
/// provider is saying "later", and the run is better off telling the
/// model that than parking it.
pub(crate) const MAX_WAIT: Duration = Duration::from_secs(10);

/// How long to wait before the retry numbered `attempt` (zero-based), given
/// what the provider said.
pub(crate) fn wait_before(attempt: usize, retry_after: Option<&str>) -> Duration {
    retry_after
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|secs| Duration::from_secs(secs).min(MAX_WAIT))
        .unwrap_or(DELAYS[attempt.min(DELAYS.len() - 1)])
}

/// Whether a response is the provider's failure rather than ours.
fn is_transient(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// What kind of failure a provider's final answer was, for `/metrics`, or
/// `None` when it was not one.
fn failure_kind(outcome: &reqwest::Result<reqwest::Response>) -> Option<&'static str> {
    match outcome {
        Ok(resp) if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => Some("rate_limited"),
        Ok(resp) if resp.status().is_server_error() => Some("server"),
        Ok(resp) if !resp.status().is_success() => Some("client"),
        Ok(_) => None,
        Err(e) if e.is_timeout() => Some("timeout"),
        Err(_) => Some("network"),
    }
}

pub(crate) trait SendWithRetry {
    /// `send`, with up to two more tries on a 429, a 5xx or a refused
    /// connection. Hands back whatever the last attempt produced.
    ///
    /// `provider` is the name `/metrics` counts its retries and failures
    /// under: one per paid API, never anything a caller typed.
    async fn send_with_retry(self, provider: &'static str) -> reqwest::Result<reqwest::Response>;
}

impl SendWithRetry for reqwest::RequestBuilder {
    async fn send_with_retry(self, provider: &'static str) -> reqwest::Result<reqwest::Response> {
        let metrics = crate::metrics::global();
        let mut attempt = 0;
        let mut request = self;
        loop {
            // A streaming body cannot be cloned; such a request gets its
            // one attempt, as before. Every provider here sends JSON, a
            // form or nothing, all of which clone.
            let again = request.try_clone();
            let outcome = request.send().await;
            let last = attempt + 1 >= ATTEMPTS || again.is_none();
            let (wait, why) = match &outcome {
                Ok(resp) if is_transient(resp.status()) && !last => {
                    let header = resp.headers().get(reqwest::header::RETRY_AFTER);
                    let retry_after = header.and_then(|v| v.to_str().ok());
                    (wait_before(attempt, retry_after), resp.status().to_string())
                }
                Err(e) if e.is_connect() && !last => (wait_before(attempt, None), e.to_string()),
                _ => {
                    if let Some(kind) = failure_kind(&outcome) {
                        metrics.provider_error(provider, kind);
                    }
                    return outcome;
                }
            };
            metrics.provider_retry(provider);
            tracing::warn!(attempt = attempt + 1, wait_ms = wait.as_millis() as u64, why, "retrying a provider request");
            tokio::time::sleep(wait).await;
            request = again.expect("checked above");
            attempt += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn hits(server: &MockServer) -> Vec<wiremock::Request> {
        server.received_requests().await.unwrap()
    }

    /// The value of one series on the process's page, or 0 before it has
    /// been counted. Each test names a provider of its own, so no other
    /// test writes to the series it reads.
    fn counted(series: &str) -> u64 {
        let page = crate::metrics::global().render();
        let start = format!("{series} ");
        page.lines().find_map(|l| l.strip_prefix(start.as_str())).map_or(0, |n| n.parse().unwrap())
    }

    fn post(server: &MockServer) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .post(format!("{}/q", server.uri()))
            .json(&serde_json::json!({"query": "wasmiddel"}))
    }

    #[tokio::test]
    async fn a_429_is_retried_with_the_same_body_and_the_answer_that_follows_is_returned() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/q"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/q"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let resp = post(&server).send_with_retry("test-rescued").await.unwrap();

        assert_eq!(resp.status(), 200);
        let seen = hits(&server).await;
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].body, seen[1].body, "the retry must carry the whole request");
        // The 429 the provider sent is counted though the call was saved.
        assert_eq!(counted("scout_provider_retries_total{provider=\"test-rescued\"}"), 1);
        assert!(!crate::metrics::global().render().contains("provider=\"test-rescued\",kind="), "a call that worked is no error");
    }

    #[tokio::test]
    async fn a_400_is_the_callers_problem_and_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/q"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&server)
            .await;

        let resp = post(&server).send_with_retry("test-refused").await.unwrap();

        assert_eq!(resp.status(), 400);
        assert_eq!(hits(&server).await.len(), 1);
        assert_eq!(counted("scout_provider_errors_total{provider=\"test-refused\",kind=\"client\"}"), 1);
        assert_eq!(counted("scout_provider_retries_total{provider=\"test-refused\"}"), 0);
    }

    #[tokio::test]
    async fn a_503_that_never_clears_is_handed_back_after_the_last_attempt() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/q"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let resp = post(&server).send_with_retry("test-down").await.unwrap();

        // The caller's own status check still runs and words the error.
        assert_eq!(resp.status(), 503);
        assert_eq!(hits(&server).await.len(), ATTEMPTS);
        // One failed call, however many times it was sent.
        assert_eq!(counted("scout_provider_errors_total{provider=\"test-down\",kind=\"server\"}"), 1);
        assert_eq!(counted("scout_provider_retries_total{provider=\"test-down\"}"), (ATTEMPTS - 1) as u64);
    }

    #[tokio::test]
    async fn a_429_that_never_clears_is_a_rate_limit_and_a_timeout_is_its_own_kind() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/q"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/slow"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(500)))
            .mount(&server)
            .await;

        let resp = post(&server).send_with_retry("test-throttled").await.unwrap();
        let slow = reqwest::Client::builder().timeout(Duration::from_millis(50)).build().unwrap();
        let err = slow.post(format!("{}/slow", server.uri())).send_with_retry("test-slow").await.unwrap_err();

        assert_eq!(resp.status(), 429);
        assert_eq!(counted("scout_provider_errors_total{provider=\"test-throttled\",kind=\"rate_limited\"}"), 1);
        assert!(err.is_timeout(), "got: {err}");
        assert_eq!(counted("scout_provider_errors_total{provider=\"test-slow\",kind=\"timeout\"}"), 1);
    }

    #[tokio::test]
    async fn a_refused_connection_ends_as_the_connection_error_it_was() {
        // A port nobody listens on: bound to learn the number, then dropped.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();

        let err = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/q"))
            .send_with_retry("test-unreachable")
            .await
            .unwrap_err();

        assert!(err.is_connect(), "got: {err}");
        assert_eq!(counted("scout_provider_errors_total{provider=\"test-unreachable\",kind=\"network\"}"), 1);
    }

    #[test]
    fn retry_after_is_honoured_up_to_a_ceiling() {
        assert_eq!(wait_before(0, Some("2")), std::time::Duration::from_secs(2));
        assert_eq!(wait_before(0, Some("600")), MAX_WAIT, "a provider cannot park a run for ten minutes");
        assert_eq!(wait_before(0, Some("soon")), DELAYS[0], "an unreadable header is no header");
        assert_eq!(wait_before(1, None), DELAYS[1]);
    }

    #[test]
    fn every_paid_provider_sends_through_the_retry() {
        // A source assertion, because the alternative is noticing in
        // production that the one client added last still fails on the
        // first 429. `fetch` and `links` are deliberately absent: they open
        // arbitrary pages, where a 503 is a bot wall and a retry is a
        // second knock on a door that just said no.
        let providers = [
            ("kagi", include_str!("tools/kagi.rs")),
            ("perplexity", include_str!("tools/perplexity.rs")),
            ("duffel", include_str!("tools/duffel.rs")),
            ("ignav", include_str!("tools/ignav.rs")),
            ("ebay", include_str!("tools/ebay.rs")),
            ("bol", include_str!("tools/bol.rs")),
            ("marktplaats", include_str!("tools/marktplaats.rs")),
        ];
        for (name, src) in providers {
            let src = src.split("#[cfg(test)]").next().unwrap();
            assert_eq!(src.matches(".send()").count(), 0, "{name} sends a request without the retry");
            let named = format!("send_with_retry(\"{name}\")");
            assert!(src.contains(&named), "{name} never sends through the retry");
            assert_eq!(
                src.matches("send_with_retry(").count(),
                src.matches(named.as_str()).count(),
                "{name} counts a request under another provider's name"
            );
        }
    }
}
