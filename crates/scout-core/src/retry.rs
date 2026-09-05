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

pub(crate) trait SendWithRetry {
    /// `send`, with up to two more tries on a 429, a 5xx or a refused
    /// connection. Hands back whatever the last attempt produced.
    async fn send_with_retry(self) -> reqwest::Result<reqwest::Response>;
}

impl SendWithRetry for reqwest::RequestBuilder {
    async fn send_with_retry(self) -> reqwest::Result<reqwest::Response> {
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
                _ => return outcome,
            };
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

        let resp = post(&server).send_with_retry().await.unwrap();

        assert_eq!(resp.status(), 200);
        let seen = hits(&server).await;
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].body, seen[1].body, "the retry must carry the whole request");
    }

    #[tokio::test]
    async fn a_400_is_the_callers_problem_and_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/q"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&server)
            .await;

        let resp = post(&server).send_with_retry().await.unwrap();

        assert_eq!(resp.status(), 400);
        assert_eq!(hits(&server).await.len(), 1);
    }

    #[tokio::test]
    async fn a_503_that_never_clears_is_handed_back_after_the_last_attempt() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/q"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let resp = post(&server).send_with_retry().await.unwrap();

        // The caller's own status check still runs and words the error.
        assert_eq!(resp.status(), 503);
        assert_eq!(hits(&server).await.len(), ATTEMPTS);
    }

    #[tokio::test]
    async fn a_refused_connection_ends_as_the_connection_error_it_was() {
        // A port nobody listens on: bound to learn the number, then dropped.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();

        let err = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/q"))
            .send_with_retry()
            .await
            .unwrap_err();

        assert!(err.is_connect(), "got: {err}");
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
            assert!(src.contains("send_with_retry()"), "{name} never sends through the retry");
        }
    }
}
