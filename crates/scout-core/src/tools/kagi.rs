use crate::retry::SendWithRetry;
use serde::{Deserialize, Serialize};

pub const KAGI_API_BASE: &str = "https://kagi.com/api";

#[derive(Debug, thiserror::Error)]
pub enum KagiError {
    #[error("{0}")]
    Budget(String),
    #[error("kagi request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("kagi api error (status {status}): {body}")]
    Api { status: u16, body: String },
    #[error("kagi returned an unexpected response: {detail}")]
    Decode { detail: String },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Deserialize)]
struct SearchResponse {
    data: SearchData,
}

#[derive(Deserialize, Default)]
struct SearchData {
    #[serde(default)]
    search: Vec<RawResult>,
}

#[derive(Deserialize)]
struct RawResult {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    snippet: Option<String>,
}

/// Strips HTML tags and decodes the handful of entities Kagi snippets use.
/// Tags are removed first, then entities are decoded with `&amp;` last so a
/// literal `&amp;lt;` in the source decodes to `&lt;`, not `<`.
fn clean_snippet(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

/// Thin client over the Kagi v1 Search API.
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
            .post(format!("{}/v1/search", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&serde_json::json!({ "query": query, "limit": limit }))
            .send_with_retry()
            .await?;
        let body: SearchResponse = decode(check(resp).await?).await?;
        Ok(body
            .data
            .search
            .into_iter()
            .map(|r| SearchResult {
                title: r.title.unwrap_or_default(),
                url: r.url.unwrap_or_default(),
                snippet: clean_snippet(&r.snippet.unwrap_or_default()),
            })
            .take(limit)
            .collect())
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

async fn decode<T: for<'de> Deserialize<'de>>(resp: reqwest::Response) -> Result<T, KagiError> {
    let text = resp.text().await?;
    serde_json::from_str(&text).map_err(|e| {
        let snippet: String = text.chars().take(200).collect();
        KagiError::Decode {
            detail: format!("{e}; body: {snippet}"),
        }
    })
}

use rig::tool::Tool;
use serde_json::json;

#[derive(Deserialize)]
pub struct SearchArgs {
    pub query: String,
    /// Other phrasings of the same search — normally translations into the
    /// user's local languages. Run concurrently with `query`, so covering
    /// three languages costs one agent turn, not three.
    #[serde(default)]
    pub also_queries: Vec<String>,
}

/// Queries per search_web call (the main one plus translations).
const MAX_QUERIES: usize = 3;
/// Merged result cap: enough to compare on, small enough to keep the model's
/// context readable.
const MERGED_LIMIT: usize = 12;

/// Web search over both engines we have. They are complementary, not
/// redundant: on the same Dutch shopping query their results overlapped by
/// 2 URLs in 20, and the small retailers with the cheap offers were split
/// between them. Kagi bills per query, Perplexity per request (any number of
/// queries), so the cheap engine carries the language fan-out.
pub struct WebSearchTool {
    pub kagi: KagiClient,
    pub perplexity: Option<super::perplexity::PerplexityClient>,
    /// Kagi's allowance, shared with search_secondhand. Perplexity is an
    /// order of magnitude cheaper and is not counted against it.
    pub budget: std::sync::Arc<super::budget::SearchBudget>,
}

impl Tool for WebSearchTool {
    const NAME: &'static str = "search_web";
    type Error = KagiError;
    type Args = SearchArgs;
    type Output = Vec<SearchResult>;

    fn description(&self) -> String {
        "Search the web. Returns result titles, URLs and snippets. \
         Use for finding products, shops, prices and reviews. Pass \
         translations of your query in also_queries to reach local shops — \
         they are searched in parallel and the results are merged, so it \
         costs no extra steps."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "web search query"},
                "also_queries": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "up to 2 translations of the same query into the user's \
                                    search languages, e.g. ['vloeibaar wasmiddel kleur', \
                                    'Flüssigwaschmittel Color'] — translate the product \
                                    terms, do not just copy the English words"
                }
            },
            "required": ["query"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let mut queries = vec![args.query.trim().to_string()];
        for q in &args.also_queries {
            let q = q.trim().to_string();
            if !q.is_empty() && !queries.contains(&q) && queries.len() < MAX_QUERIES {
                queries.push(q);
            }
        }
        // Kagi bills per query, so translations are the first thing to drop
        // when the allowance runs low — the user's own phrasing is
        // queries[0]. A spent budget is not fatal any more: Perplexity keeps
        // searching, all languages included, for a fraction of the price.
        let granted = self.budget.claim(queries.len());
        tracing::debug!(
            kagi_queries = granted,
            budget_left = self.budget.remaining(),
            perplexity = self.perplexity.is_some(),
            "web search"
        );
        if granted == 0 && self.perplexity.is_none() {
            return Err(KagiError::Budget(format!(
                "search budget spent ({} queries per request) — answer from the results you \
                 already have instead of searching again",
                super::budget::QUERIES_PER_REQUEST
            )));
        }

        // One Kagi query: keep the old depth. Several: less from each, since
        // the merged list is what the model reads. Counted on what the budget
        // granted, not on what was asked for.
        let per_query = if granted <= 1 { 10 } else { 6 };
        let kagi = futures::future::join_all(
            queries.iter().take(granted).map(|q| self.kagi.search(q, per_query)),
        );
        let (kagi, perplexity) = match &self.perplexity {
            Some(p) => {
                let (k, p) = futures::future::join(kagi, p.search(&queries, MERGED_LIMIT)).await;
                (k, Some(p))
            }
            None => (kagi.await, None),
        };

        // Kagi first: it carries the user's own phrasing and the small-shop
        // long tail, and a duplicate URL keeps the first snippet seen.
        let mut merged: Vec<SearchResult> = Vec::new();
        let mut first_error = None;
        let mut push = |results: Vec<SearchResult>| {
            for r in results {
                if !merged.iter().any(|m| m.url == r.url) {
                    merged.push(r);
                }
            }
        };
        for answer in kagi {
            match answer {
                Ok(results) => push(results),
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
        match perplexity {
            Some(Ok(results)) => push(results),
            // One engine failing is not worth losing the other's results.
            Some(Err(e)) => {
                first_error.get_or_insert(KagiError::Api { status: 0, body: e.to_string() });
            }
            None => {}
        }

        match first_error {
            Some(e) if merged.is_empty() => Err(e),
            _ => {
                merged.truncate(MERGED_LIMIT);
                Ok(merged)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn client(server: &MockServer) -> KagiClient {
        KagiClient::new(reqwest::Client::new(), "test-key".to_string(), server.uri())
    }

    #[tokio::test]
    async fn search_parses_results_and_cleans_snippets() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .and(body_json(json!({"query": "usb hub", "limit": 10})))
            .and(header("Authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"id": "x", "node": "n", "ms": 5},
                "data": {
                    "search": [
                        {
                            "url": "https://a.example",
                            "title": "Hub A",
                            "snippet": "a <strong>usb</strong> &#39;hub&#39;",
                            "props": {}
                        },
                        {"url": "https://b.example", "title": "Hub B", "snippet": "cheap hub"}
                    ],
                    "related_search": ["usb hub 3.0"]
                }
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
                    snippet: "a usb 'hub'".into()
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
    async fn search_truncates_to_limit_even_when_server_ignores_it() {
        let server = MockServer::start().await;
        let many: Vec<_> = (0..10)
            .map(|i| json!({"title": format!("R{i}"), "url": "https://x", "snippet": ""}))
            .collect();
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"search": many}})))
            .mount(&server)
            .await;

        let results = client(&server).await.search("q", 3).await.unwrap();
        assert_eq!(results.len(), 3);
    }

    #[tokio::test]
    async fn search_surfaces_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "meta": {"id": "x"},
                "data": null,
                "errors": [{"code": "401", "message": "Invalid API key", "url": ""}]
            })))
            .mount(&server)
            .await;

        let err = client(&server).await.search("q", 5).await.unwrap_err();
        assert!(matches!(err, KagiError::Api { status: 401, .. }), "got: {err}");
    }

    #[tokio::test]
    async fn search_reports_unexpected_body_as_decode_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let err = client(&server).await.search("q", 5).await.unwrap_err();
        assert!(matches!(err, KagiError::Decode { .. }), "got: {err}");
        assert!(err.to_string().contains("unexpected response"), "got: {err}");
    }

    #[tokio::test]
    async fn missing_search_key_is_empty() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"related_search": []}
            })))
            .mount(&server)
            .await;

        let results = client(&server).await.search("q", 5).await.unwrap();
        assert_eq!(results, Vec::new());
    }

    #[tokio::test]
    async fn search_tool_calls_through() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"search": [{"title": "T", "url": "https://u", "snippet": "s"}]}
            })))
            .mount(&server)
            .await;

        let tool = WebSearchTool { kagi: client(&server).await, perplexity: None, budget: Default::default() };
        let out = tool
            .call(SearchArgs { query: "x".into(), also_queries: Vec::new() })
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert!(!tool.description().is_empty());
        assert_eq!(WebSearchTool::NAME, "search_web");
    }

    #[tokio::test]
    async fn translated_queries_run_in_parallel_and_merge() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .and(body_json(json!({"query": "laundry detergent", "limit": 6})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"search": [
                {"title": "EN shop", "url": "https://a.example", "snippet": ""},
                {"title": "shared", "url": "https://shared.example", "snippet": "english"}
            ]}})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .and(body_json(json!({"query": "wasmiddel", "limit": 6})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"search": [
                {"title": "shared", "url": "https://shared.example", "snippet": "dutch"},
                {"title": "bol.com", "url": "https://bol.example", "snippet": ""}
            ]}})))
            .mount(&server)
            .await;

        let out = WebSearchTool { kagi: client(&server).await, perplexity: None, budget: Default::default() }
            .call(SearchArgs {
                query: "laundry detergent".into(),
                also_queries: vec!["wasmiddel".into(), "  ".into()],
            })
            .await
            .unwrap();

        // merged, deduped by url, and the user's own phrasing wins the dupe
        assert_eq!(
            out.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
            vec!["https://a.example", "https://shared.example", "https://bol.example"]
        );
        assert_eq!(out[1].snippet, "english");
    }

    /// A Perplexity stand-in on the same mock server, so both engines can be
    /// driven from one test.
    async fn pplx(server: &MockServer) -> crate::tools::perplexity::PerplexityClient {
        crate::tools::perplexity::PerplexityClient::new(
            reqwest::Client::new(),
            "p".to_string(),
            server.uri(),
        )
    }

    #[tokio::test]
    async fn both_engines_are_merged_kagi_first() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"search": [
                {"title": "small shop", "url": "https://tradefix.nl/p", "snippet": "kagi"},
                {"title": "both", "url": "https://bol.com/p", "snippet": "from kagi"}
            ]}})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": [
                {"title": "both", "url": "https://bol.com/p", "snippet": "from perplexity"},
                {"title": "other shop", "url": "https://koala.nl/p", "snippet": "pplx"}
            ]})))
            .mount(&server)
            .await;

        let out = WebSearchTool {
            kagi: client(&server).await,
            perplexity: Some(pplx(&server).await),
            budget: Default::default(),
        }
        .call(SearchArgs { query: "wasmiddel".into(), also_queries: vec!["detergent".into()] })
        .await
        .unwrap();

        // deduped by url, Kagi's ordering and snippet win the shared hit
        assert_eq!(
            out.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
            vec!["https://tradefix.nl/p", "https://bol.com/p", "https://koala.nl/p"]
        );
        assert_eq!(out[1].snippet, "from kagi");
    }

    #[tokio::test]
    async fn a_spent_kagi_budget_falls_back_to_perplexity() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(ResponseTemplate::new(500).set_body_string("kagi must not be called"))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": [
                {"title": "still searching", "url": "https://koala.nl/p", "snippet": ""}
            ]})))
            .mount(&server)
            .await;

        let out = WebSearchTool {
            kagi: client(&server).await,
            perplexity: Some(pplx(&server).await),
            budget: std::sync::Arc::new(crate::tools::budget::SearchBudget::new(0)),
        }
        .call(SearchArgs { query: "q".into(), also_queries: Vec::new() })
        .await
        .unwrap();
        assert_eq!(out.len(), 1);
    }

    #[tokio::test]
    async fn the_search_budget_drops_translations_first_then_refuses() {
        let server = MockServer::start().await;
        // Only the main query is mocked; if a translation ran with 1 left,
        // the limit would be 6 and this mock would not match.
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .and(body_json(json!({"query": "q", "limit": 10})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"search": [
                {"title": "T", "url": "https://u", "snippet": ""}
            ]}})))
            .mount(&server)
            .await;

        let tool = WebSearchTool {
            kagi: client(&server).await,
            perplexity: None,
            budget: std::sync::Arc::new(crate::tools::budget::SearchBudget::new(1)),
        };
        // asked for 3 queries, granted 1: the user's own phrasing survives
        let out = tool
            .call(SearchArgs {
                query: "q".into(),
                also_queries: vec!["vertaling".into(), "Übersetzung".into()],
            })
            .await
            .unwrap();
        assert_eq!(out.len(), 1);

        let err = tool
            .call(SearchArgs { query: "q".into(), also_queries: Vec::new() })
            .await
            .unwrap_err();
        assert!(matches!(err, KagiError::Budget(_)), "got: {err}");
        assert!(err.to_string().contains("already have"), "got: {err}");
    }

    #[tokio::test]
    async fn a_failing_translation_does_not_sink_the_search() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .and(body_json(json!({"query": "ok", "limit": 6})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"search": [
                {"title": "T", "url": "https://u", "snippet": ""}
            ]}})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .and(body_json(json!({"query": "boom", "limit": 6})))
            .respond_with(ResponseTemplate::new(500).set_body_string("nope"))
            .mount(&server)
            .await;

        let tool = WebSearchTool { kagi: client(&server).await, perplexity: None, budget: Default::default() };
        let out = tool
            .call(SearchArgs { query: "ok".into(), also_queries: vec!["boom".into()] })
            .await
            .unwrap();
        assert_eq!(out.len(), 1);

        // but when every query fails the model must see the error
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .and(body_json(json!({"query": "kaboom", "limit": 6})))
            .respond_with(ResponseTemplate::new(500).set_body_string("nope"))
            .mount(&server)
            .await;
        let err = tool
            .call(SearchArgs { query: "boom".into(), also_queries: vec!["kaboom".into()] })
            .await
            .unwrap_err();
        assert!(matches!(err, KagiError::Api { status: 500, .. }), "got: {err}");
    }

    #[test]
    fn clean_snippet_strips_tags() {
        assert_eq!(clean_snippet("a <strong>bold</strong> word"), "a bold word");
    }

    #[test]
    fn clean_snippet_decodes_entities() {
        assert_eq!(clean_snippet("Tom &amp; Jerry &#39;s &quot;show&quot;"), "Tom & Jerry 's \"show\"");
        assert_eq!(clean_snippet("a&nbsp;b"), "a b");
    }

    #[test]
    fn clean_snippet_does_not_double_decode_amp() {
        // "&amp;lt;" must become "&lt;", not "<" — amp decodes last.
        assert_eq!(clean_snippet("&amp;lt;"), "&lt;");
    }
}
