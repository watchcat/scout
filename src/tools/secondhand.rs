use super::kagi::{KagiClient, SearchArgs, SearchResult};
use rig::tool::Tool;
use serde::Serialize;
use serde_json::json;
use std::convert::Infallible;

#[derive(Debug, Serialize)]
pub struct PlatformResults {
    pub platform: String,
    pub results: Vec<SearchResult>,
    /// Set when this platform's search failed; other platforms still return.
    pub error: Option<String>,
}

pub struct SecondhandSearchTool {
    pub client: KagiClient,
    pub sites: Vec<String>,
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
            let site = site.clone();
            let query = format!("site:{site} {}", args.query);
            async move {
                match client.search(&query, 5).await {
                    Ok(results) => PlatformResults { platform: site, results, error: None },
                    Err(e) => PlatformResults {
                        platform: site,
                        results: Vec::new(),
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
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn tool(server: &MockServer, sites: &[&str]) -> SecondhandSearchTool {
        SecondhandSearchTool {
            client: KagiClient::new(
                reqwest::Client::new(),
                "k".to_string(),
                server.uri(),
            ),
            sites: sites.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[tokio::test]
    async fn queries_are_site_scoped_and_grouped() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/search"))
            .and(query_param("q", "site:ebay.com bike"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"t": 0, "title": "eBay bike", "url": "https://e", "snippet": ""}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/search"))
            .and(query_param("q", "site:vinted.com bike"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"t": 0, "title": "Vinted bike", "url": "https://v", "snippet": ""}]
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
        Mock::given(method("GET"))
            .and(path("/v1/search"))
            .and(query_param("q", "site:good.com widget"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"t": 0, "title": "ok", "url": "https://g", "snippet": ""}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/search"))
            .and(query_param("q", "site:bad.com widget"))
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
}
