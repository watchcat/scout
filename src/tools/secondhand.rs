use super::kagi::{KagiClient, SearchArgs, SearchResult};
use rig::tool::Tool;
use serde::Serialize;
use serde_json::json;
use std::convert::Infallible;

#[derive(Debug, Serialize)]
pub struct PlatformResults {
    pub platform: String,
    pub results: Vec<SearchResult>,
    /// Search results whose links answered 404/410 (listing deleted) and
    /// were removed before reaching the model.
    pub dead_links_removed: usize,
    /// Set when this platform's search failed; other platforms still return.
    pub error: Option<String>,
}

/// Profile fact key holding a user's personal marketplace list.
pub const SITES_FACT_KEY: &str = "secondhand_sites";
/// Each site costs one Kagi query per search; keep the fan-out bounded.
const MAX_SITES: usize = 8;

/// The marketplace list to search for this user: their `secondhand_sites`
/// profile fact when present (comma-separated domains), else the configured
/// default.
pub fn effective_sites(facts: &[(String, String)], default: &[String]) -> Vec<String> {
    facts
        .iter()
        .find(|(k, _)| k == SITES_FACT_KEY)
        .map(|(_, v)| parse_sites(v))
        .filter(|sites| !sites.is_empty())
        .unwrap_or_else(|| default.to_vec())
}

/// Normalize a comma/whitespace-separated domain list: strip scheme, `www.`
/// and paths, lowercase, dedupe, cap at MAX_SITES.
pub fn parse_sites(value: &str) -> Vec<String> {
    let mut sites = Vec::new();
    for raw in value.split([',', ' ', '\n']) {
        let site = raw
            .trim()
            .to_lowercase()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_start_matches("www.")
            .split('/')
            .next()
            .unwrap_or_default()
            .to_string();
        if !site.is_empty() && site.contains('.') && !sites.contains(&site) {
            sites.push(site);
            if sites.len() >= MAX_SITES {
                break;
            }
        }
    }
    sites
}

pub struct SecondhandSearchTool {
    pub client: KagiClient,
    pub http: reqwest::Client,
    pub sites: Vec<String>,
}

/// True only when the server definitively says the page no longer exists
/// (404/410 — e.g. a deleted Marktplaats listing answers 410). Bot walls
/// (403/503) and network errors are NOT dead: we just can't verify those.
async fn is_dead_link(http: &reqwest::Client, url: &str) -> bool {
    match super::fetch::browser_get(http, url).send().await {
        Ok(resp) => matches!(resp.status().as_u16(), 404 | 410),
        Err(_) => false,
    }
}

impl Tool for SecondhandSearchTool {
    const NAME: &'static str = "search_secondhand";
    type Error = Infallible;
    type Args = SearchArgs;
    type Output = Vec<PlatformResults>;

    fn description(&self) -> String {
        format!(
            "Search second-hand marketplaces ({}) for a product, all platforms \
             in parallel. Returns results grouped by platform. Use when the user \
             wants used items or second-hand is a sensible option.",
            self.sites.join(", ")
        )
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "product search query"}
            },
            "required": ["query"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let searches = self.sites.iter().map(|site| {
            let client = self.client.clone();
            let http = self.http.clone();
            let site = site.clone();
            let query = format!("site:{site} {}", args.query);
            async move {
                match client.search(&query, 5).await {
                    Ok(results) => {
                        // Verify every hit concurrently; drop deleted listings.
                        let alive = futures::future::join_all(results.iter().map(|r| {
                            let http = http.clone();
                            let url = r.url.clone();
                            async move { !is_dead_link(&http, &url).await }
                        }))
                        .await;
                        let total = results.len();
                        let results: Vec<SearchResult> = results
                            .into_iter()
                            .zip(alive)
                            .filter_map(|(r, keep)| keep.then_some(r))
                            .collect();
                        PlatformResults {
                            platform: site,
                            dead_links_removed: total - results.len(),
                            results,
                            error: None,
                        }
                    }
                    Err(e) => PlatformResults {
                        platform: site,
                        results: Vec::new(),
                        dead_links_removed: 0,
                        error: Some(e.to_string()),
                    },
                }
            }
        });
        Ok(futures::future::join_all(searches).await)
    }
}

#[cfg(test)]
mod tests {
    use super::{effective_sites, parse_sites, SITES_FACT_KEY};

    #[test]
    fn parse_sites_normalizes_and_dedupes() {
        assert_eq!(
            parse_sites("https://www.eBay.com/sch/x, vinted.nl , ebay.com\nmarktplaats.nl"),
            vec!["ebay.com", "vinted.nl", "marktplaats.nl"]
        );
        assert!(parse_sites("not-a-domain, ,").is_empty());
    }

    #[test]
    fn parse_sites_caps_the_list() {
        let many = (0..20).map(|i| format!("site{i}.com")).collect::<Vec<_>>().join(",");
        assert_eq!(parse_sites(&many).len(), 8);
    }

    #[test]
    fn effective_sites_prefers_valid_fact_over_default() {
        let default = vec!["ebay.com".to_string()];
        let facts = vec![(SITES_FACT_KEY.to_string(), "vinted.pl, allegro.pl".to_string())];
        assert_eq!(effective_sites(&facts, &default), vec!["vinted.pl", "allegro.pl"]);
        // no fact, or a fact that parses to nothing → default
        assert_eq!(effective_sites(&[], &default), default);
        let junk = vec![(SITES_FACT_KEY.to_string(), "garbage".to_string())];
        assert_eq!(effective_sites(&junk, &default), default);
    }

    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn tool(server: &MockServer, sites: &[&str]) -> SecondhandSearchTool {
        SecondhandSearchTool {
            client: KagiClient::new(
                reqwest::Client::new(),
                "k".to_string(),
                server.uri(),
            ),
            http: reqwest::Client::new(),
            sites: sites.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[tokio::test]
    async fn queries_are_site_scoped_and_grouped() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .and(body_json(json!({"query": "site:ebay.com bike", "limit": 5})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"search": [{"title": "eBay bike", "url": "https://e", "snippet": ""}]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .and(body_json(json!({"query": "site:vinted.com bike", "limit": 5})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"search": [{"title": "Vinted bike", "url": "https://v", "snippet": ""}]}
            })))
            .mount(&server)
            .await;

        let out = tool(&server, &["ebay.com", "vinted.com"])
            .call(SearchArgs { query: "bike".into() })
            .await
            .unwrap();

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].platform, "ebay.com");
        assert_eq!(out[0].results[0].title, "eBay bike");
        assert!(out[0].error.is_none());
        assert_eq!(out[1].platform, "vinted.com");
        assert_eq!(out[1].results[0].title, "Vinted bike");
    }

    #[tokio::test]
    async fn one_platform_failing_does_not_sink_the_others() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .and(body_json(json!({"query": "site:good.com widget", "limit": 5})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"search": [{"title": "ok", "url": "https://g", "snippet": ""}]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .and(body_json(json!({"query": "site:bad.com widget", "limit": 5})))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let out = tool(&server, &["good.com", "bad.com"])
            .call(SearchArgs { query: "widget".into() })
            .await
            .unwrap();

        assert_eq!(out[0].results.len(), 1);
        assert!(out[0].error.is_none());
        assert!(out[1].results.is_empty());
        assert!(out[1].error.as_deref().unwrap().contains("500"));
    }

    #[tokio::test]
    async fn deleted_listings_are_dropped_and_counted() {
        let server = MockServer::start().await;
        let alive_url = format!("{}/listing/alive", server.uri());
        let dead_url = format!("{}/listing/dead", server.uri());
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"search": [
                    {"title": "still there", "url": alive_url, "snippet": ""},
                    {"title": "already sold", "url": dead_url, "snippet": ""}
                ]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/listing/alive"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>ok</html>"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/listing/dead"))
            .respond_with(ResponseTemplate::new(410))
            .mount(&server)
            .await;

        let out = tool(&server, &["marktplaats.nl"])
            .call(SearchArgs { query: "s25 ultra".into() })
            .await
            .unwrap();

        assert_eq!(out[0].results.len(), 1);
        assert_eq!(out[0].results[0].title, "still there");
        assert_eq!(out[0].dead_links_removed, 1);
        assert!(out[0].error.is_none());
    }

    #[tokio::test]
    async fn bot_walls_and_unreachable_links_are_kept() {
        let server = MockServer::start().await;
        let blocked_url = format!("{}/listing/blocked", server.uri());
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"search": [
                    {"title": "bot-walled shop", "url": blocked_url, "snippet": ""},
                    {"title": "unreachable host", "url": "https://nx.invalid/x", "snippet": ""}
                ]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/listing/blocked"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let out = tool(&server, &["ebay.com"])
            .call(SearchArgs { query: "widget".into() })
            .await
            .unwrap();

        // 503 (anti-bot) and network failures are "can't verify", not "gone".
        assert_eq!(out[0].results.len(), 2);
        assert_eq!(out[0].dead_links_removed, 0);
    }
}
