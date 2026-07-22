use serde::{Deserialize, Serialize};

pub const KAGI_API_BASE: &str = "https://kagi.com/api";

#[derive(Debug, thiserror::Error)]
pub enum KagiError {
    #[error("kagi request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("kagi api error (status {status}): {body}")]
    Api { status: u16, body: String },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Deserialize)]
struct SearchResponse {
    data: Vec<RawResult>,
}

/// `t == 0` is a search result; other values (related searches etc.) are skipped.
#[derive(Deserialize)]
struct RawResult {
    t: i64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    snippet: Option<String>,
}

#[derive(Deserialize)]
struct SummarizeResponse {
    data: SummarizeData,
}

#[derive(Deserialize)]
struct SummarizeData {
    output: String,
}

/// Thin client over the Kagi Search and Universal Summarizer APIs.
/// `base_url` is injectable so tests can point it at a mock server.
#[derive(Clone)]
pub struct KagiClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl KagiClient {
    pub fn new(http: reqwest::Client, api_key: String, base_url: String) -> Self {
        Self { http, api_key, base_url }
    }

    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>, KagiError> {
        let resp = self
            .http
            .get(format!("{}/v1/search", self.base_url))
            .header("Authorization", format!("Bot {}", self.api_key))
            .query(&[("q", query)])
            .send()
            .await?;
        let body: SearchResponse = check(resp).await?.json().await?;
        Ok(body
            .data
            .into_iter()
            .filter(|r| r.t == 0)
            .map(|r| SearchResult {
                title: r.title.unwrap_or_default(),
                url: r.url.unwrap_or_default(),
                snippet: r.snippet.unwrap_or_default(),
            })
            .take(limit)
            .collect())
    }

    pub async fn summarize(&self, url: &str) -> Result<String, KagiError> {
        let resp = self
            .http
            .get(format!("{}/v0/summarize", self.base_url))
            .header("Authorization", format!("Bot {}", self.api_key))
            .query(&[("url", url), ("summary_type", "summary")])
            .send()
            .await?;
        let body: SummarizeResponse = check(resp).await?.json().await?;
        Ok(body.data.output)
    }
}

async fn check(resp: reqwest::Response) -> Result<reqwest::Response, KagiError> {
    let status = resp.status();
    if status.is_success() {
        Ok(resp)
    } else {
        Err(KagiError::Api {
            status: status.as_u16(),
            body: resp.text().await.unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn client(server: &MockServer) -> KagiClient {
        KagiClient::new(reqwest::Client::new(), "test-key".to_string(), server.uri())
    }

    #[tokio::test]
    async fn search_parses_results_and_skips_non_results() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/search"))
            .and(query_param("q", "usb hub"))
            .and(header("Authorization", "Bot test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"id": "x", "node": "n", "ms": 5},
                "data": [
                    {"t": 0, "title": "Hub A", "url": "https://a.example", "snippet": "nice hub"},
                    {"t": 1, "list": ["usb hub 3.0"]},
                    {"t": 0, "title": "Hub B", "url": "https://b.example", "snippet": "cheap hub"}
                ]
            })))
            .mount(&server)
            .await;

        let results = client(&server).await.search("usb hub", 10).await.unwrap();
        assert_eq!(
            results,
            vec![
                SearchResult {
                    title: "Hub A".into(),
                    url: "https://a.example".into(),
                    snippet: "nice hub".into()
                },
                SearchResult {
                    title: "Hub B".into(),
                    url: "https://b.example".into(),
                    snippet: "cheap hub".into()
                },
            ]
        );
    }

    #[tokio::test]
    async fn search_truncates_to_limit() {
        let server = MockServer::start().await;
        let many: Vec<_> = (0..10)
            .map(|i| json!({"t": 0, "title": format!("R{i}"), "url": "https://x", "snippet": ""}))
            .collect();
        Mock::given(method("GET"))
            .and(path("/v1/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": many})))
            .mount(&server)
            .await;

        let results = client(&server).await.search("q", 3).await.unwrap();
        assert_eq!(results.len(), 3);
    }

    #[tokio::test]
    async fn search_surfaces_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/search"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;

        let err = client(&server).await.search("q", 5).await.unwrap_err();
        assert!(matches!(err, KagiError::Api { status: 401, .. }), "got: {err}");
    }

    #[tokio::test]
    async fn summarize_returns_output() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v0/summarize"))
            .and(query_param("url", "https://shop.example/p/1"))
            .and(query_param("summary_type", "summary"))
            .and(header("Authorization", "Bot test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"id": "x"},
                "data": {"output": "A fine product.", "tokens": 42}
            })))
            .mount(&server)
            .await;

        let out = client(&server).await.summarize("https://shop.example/p/1").await.unwrap();
        assert_eq!(out, "A fine product.");
    }
}
