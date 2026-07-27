use serde::Deserialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const EBAY_API_BASE: &str = "https://api.ebay.com";
/// Refresh the OAuth token this long before eBay's stated expiry.
const TOKEN_MARGIN: Duration = Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
pub enum EbayError {
    #[error("ebay request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("ebay api error (status {status}): {body}")]
    Api { status: u16, body: String },
    #[error("ebay returned an unexpected response: {detail}")]
    Decode { detail: String },
}

/// A live listing from the eBay Browse API.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct EbayItem {
    pub title: String,
    pub url: String,
    pub price: Option<String>, // "49.99 EUR"
    pub condition: Option<String>,
}

/// Browse API client using the client-credentials OAuth flow; the token
/// (~2h validity) is cached and refreshed with a safety margin.
/// `base_url` is injectable for tests.
#[derive(Clone)]
pub struct EbayClient {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
    marketplace: String,
    base_url: String,
    token: Arc<Mutex<Option<(String, Instant)>>>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default, rename = "itemSummaries")]
    item_summaries: Vec<RawItem>,
}

#[derive(Deserialize)]
struct RawItem {
    #[serde(default)]
    title: Option<String>,
    #[serde(default, rename = "itemWebUrl")]
    item_web_url: Option<String>,
    #[serde(default)]
    price: Option<RawPrice>,
    #[serde(default)]
    condition: Option<String>,
}

#[derive(Deserialize)]
struct RawPrice {
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    currency: Option<String>,
}

impl EbayClient {
    pub fn new(
        http: reqwest::Client,
        client_id: String,
        client_secret: String,
        marketplace: String,
        base_url: String,
    ) -> Self {
        Self {
            http,
            client_id,
            client_secret,
            marketplace,
            base_url,
            token: Arc::new(Mutex::new(None)),
        }
    }

    async fn token(&self) -> Result<String, EbayError> {
        if let Some((token, expires_at)) = self.token.lock().unwrap().clone() {
            if Instant::now() < expires_at {
                return Ok(token);
            }
        }
        let resp = self
            .http
            .post(format!("{}/identity/v1/oauth2/token", self.base_url))
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .form(&[
                ("grant_type", "client_credentials"),
                ("scope", "https://api.ebay.com/oauth/api_scope"),
            ])
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(EbayError::Api { status: status.as_u16(), body: text });
        }
        let parsed: TokenResponse = serde_json::from_str(&text).map_err(|e| EbayError::Decode {
            detail: format!("token response: {e}"),
        })?;
        let expires_at =
            Instant::now() + Duration::from_secs(parsed.expires_in).saturating_sub(TOKEN_MARGIN);
        *self.token.lock().unwrap() = Some((parsed.access_token.clone(), expires_at));
        Ok(parsed.access_token)
    }

    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<EbayItem>, EbayError> {
        let token = self.token().await?;
        let resp = self
            .http
            .get(format!("{}/buy/browse/v1/item_summary/search", self.base_url))
            .bearer_auth(token)
            .header("X-EBAY-C-MARKETPLACE-ID", &self.marketplace)
            .query(&[("q", query), ("limit", &limit.to_string())])
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(EbayError::Api { status: status.as_u16(), body: text });
        }
        let parsed: SearchResponse = serde_json::from_str(&text).map_err(|e| EbayError::Decode {
            detail: format!("search response: {e}"),
        })?;
        Ok(parsed
            .item_summaries
            .into_iter()
            .filter_map(|raw| {
                let url = raw.item_web_url?;
                Some(EbayItem {
                    title: raw.title.unwrap_or_default(),
                    url,
                    price: raw.price.and_then(|p| match (p.value, p.currency) {
                        (Some(v), Some(c)) => Some(format!("{v} {c}")),
                        (Some(v), None) => Some(v),
                        _ => None,
                    }),
                    condition: raw.condition,
                })
            })
            .take(limit)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> EbayClient {
        EbayClient::new(
            reqwest::Client::new(),
            "app-id".to_string(),
            "cert-id".to_string(),
            "EBAY_NL".to_string(),
            server.uri(),
        )
    }

    async fn mount_token(server: &MockServer, expect: u64) {
        Mock::given(method("POST"))
            .and(path("/identity/v1/oauth2/token"))
            // base64("app-id:cert-id")
            .and(header("Authorization", "Basic YXBwLWlkOmNlcnQtaWQ="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "tok-1", "expires_in": 7200, "token_type": "Application Access Token"
            })))
            .expect(expect)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn searches_with_cached_token_and_maps_items() {
        let server = MockServer::start().await;
        mount_token(&server, 1).await; // second search must reuse the token
        Mock::given(method("GET"))
            .and(path("/buy/browse/v1/item_summary/search"))
            .and(query_param("q", "usb hub"))
            .and(header("Authorization", "Bearer tok-1"))
            .and(header("X-EBAY-C-MARKETPLACE-ID", "EBAY_NL"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 2,
                "itemSummaries": [
                    {"title": "USB hub 3.0", "itemWebUrl": "https://ebay.nl/itm/1",
                     "price": {"value": "12.50", "currency": "EUR"}, "condition": "Used"},
                    {"title": "no url item"}
                ]
            })))
            .mount(&server)
            .await;

        let c = client(&server);
        let items = c.search("usb hub", 5).await.unwrap();
        assert_eq!(
            items,
            vec![EbayItem {
                title: "USB hub 3.0".into(),
                url: "https://ebay.nl/itm/1".into(),
                price: Some("12.50 EUR".into()),
                condition: Some("Used".into()),
            }]
        );
        // second call: token endpoint must NOT be hit again (expect(1) above)
        let again = c.search("usb hub", 5).await.unwrap();
        assert_eq!(again.len(), 1);
    }

    #[tokio::test]
    async fn bad_credentials_surface_as_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/identity/v1/oauth2/token"))
            .respond_with(ResponseTemplate::new(401).set_body_string("invalid_client"))
            .mount(&server)
            .await;

        let err = client(&server).search("x", 5).await.unwrap_err();
        assert!(matches!(err, EbayError::Api { status: 401, .. }), "got: {err}");
    }

    #[tokio::test]
    async fn empty_results_are_ok() {
        let server = MockServer::start().await;
        mount_token(&server, 1).await;
        Mock::given(method("GET"))
            .and(path("/buy/browse/v1/item_summary/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"total": 0})))
            .mount(&server)
            .await;

        assert!(client(&server).search("nothing", 5).await.unwrap().is_empty());
    }
}
