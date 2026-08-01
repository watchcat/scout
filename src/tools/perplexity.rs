use super::kagi::SearchResult;
use serde::Deserialize;

pub const PERPLEXITY_API_BASE: &str = "https://api.perplexity.ai";
/// Queries per request the API accepts. Billing is per request, not per
/// query, so a fan-out inside one call is close to free.
pub const MAX_QUERIES: usize = 5;

#[derive(Debug, thiserror::Error)]
pub enum PerplexityError {
    #[error("perplexity request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("perplexity api error (status {status}): {body}")]
    Api { status: u16, body: String },
    #[error("perplexity returned an unexpected response: {detail}")]
    Decode { detail: String },
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<RawResult>,
}

#[derive(Deserialize)]
struct RawResult {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    snippet: Option<String>,
    /// Publication date when the index has one; used as a freshness hint in
    /// the snippet, since a two-year-old price page is worth less.
    #[serde(default)]
    date: Option<String>,
}

/// Perplexity Search API client. Complements Kagi rather than replacing it:
/// on the same query the two indexes overlap by about a tenth, and each
/// finds small retailers the other misses.
///
/// `base_url` is injectable for tests.
#[derive(Clone)]
pub struct PerplexityClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl PerplexityClient {
    pub fn new(http: reqwest::Client, api_key: String, base_url: String) -> Self {
        Self { http, api_key, base_url }
    }

    /// Runs every query in ONE billed request. `max_results` is the total
    /// across all of them, not per query.
    pub async fn search(
        &self,
        queries: &[String],
        max_results: usize,
    ) -> Result<Vec<SearchResult>, PerplexityError> {
        let queries: Vec<&String> = queries.iter().take(MAX_QUERIES).collect();
        let resp = self
            .http
            .post(format!("{}/search", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&serde_json::json!({ "query": queries, "max_results": max_results }))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(PerplexityError::Api { status: status.as_u16(), body: text });
        }
        let body: SearchResponse =
            serde_json::from_str(&text).map_err(|e| PerplexityError::Decode {
                detail: format!("{e}; body: {}", text.chars().take(200).collect::<String>()),
            })?;
        Ok(body
            .results
            .into_iter()
            .filter_map(|r| {
                let url = r.url.filter(|u| !u.is_empty())?;
                let snippet = r.snippet.unwrap_or_default();
                Some(SearchResult {
                    title: r.title.unwrap_or_default(),
                    url,
                    snippet: match r.date.filter(|d| !d.is_empty()) {
                        Some(date) => format!("{snippet} · page dated {date}"),
                        None => snippet,
                    },
                })
            })
            .take(max_results)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> PerplexityClient {
        PerplexityClient::new(reqwest::Client::new(), "pplx-key".to_string(), server.uri())
    }

    #[tokio::test]
    async fn every_query_travels_in_one_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .and(header("Authorization", "Bearer pplx-key"))
            .and(body_json(json!({"query": ["wasmiddel", "detergent"], "max_results": 12})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "req-1",
                "results": [
                    {"title": "Shop", "url": "https://shop.nl/p", "snippet": "€5",
                     "date": "2026-07-01", "last_updated": "2026-07-02"},
                    {"title": "No date", "url": "https://b.nl/p", "snippet": "€6", "date": null},
                    {"title": "No url", "snippet": "ignored"}
                ]
            })))
            // exactly one billed request for both queries
            .expect(1)
            .mount(&server)
            .await;

        let out = client(&server)
            .search(&["wasmiddel".to_string(), "detergent".to_string()], 12)
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].url, "https://shop.nl/p");
        assert_eq!(out[0].snippet, "€5 · page dated 2026-07-01");
        assert_eq!(out[1].snippet, "€6");
    }

    #[tokio::test]
    async fn queries_beyond_the_api_limit_are_dropped() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .and(body_json(json!({
                "query": ["q1", "q2", "q3", "q4", "q5"], "max_results": 5
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": []})))
            .mount(&server)
            .await;

        let queries: Vec<String> = (1..=7).map(|i| format!("q{i}")).collect();
        assert!(client(&server).search(&queries, 5).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn errors_surface_with_status_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;

        let err = client(&server).search(&["q".to_string()], 5).await.unwrap_err();
        assert!(matches!(err, PerplexityError::Api { status: 401, .. }), "got: {err}");

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let err = client(&server).search(&["q".to_string()], 5).await.unwrap_err();
        assert!(matches!(err, PerplexityError::Decode { .. }), "got: {err}");
    }
}
