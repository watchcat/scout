use serde::Deserialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const BOL_API_BASE: &str = "https://api.bol.com";
pub const BOL_LOGIN_BASE: &str = "https://login.bol.com";
/// bol tokens live 299 seconds and the docs warn that fetching one per call
/// gets your IP blocked, so the cache is required rather than an optimisation.
const TOKEN_MARGIN: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum BolError {
    #[error("bol request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("bol api error (status {status}): {body}")]
    Api { status: u16, body: String },
    #[error("bol returned an unexpected response: {detail}")]
    Decode { detail: String },
}

/// A product from the bol catalogue.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct BolItem {
    pub title: String,
    pub url: String,
    pub ean: Option<String>,
    /// Formatted for the model, e.g. "23.95 EUR".
    pub price: Option<String>,
    /// bol's own delivery wording ("Voor 23:59 besteld, morgen in huis").
    /// It states delivery *time*, not cost — shipping stays unknown.
    pub delivery: Option<String>,
}

/// Marketing Catalog API client (the affiliate-facing one). The Retailer API
/// covers a seller's own listings; this one searches the catalogue a buyer
/// sees, which is what a shopping assistant needs.
///
/// `api_base`/`login_base` are injectable for tests.
#[derive(Clone)]
pub struct BolClient {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
    country: String,
    api_base: String,
    login_base: String,
    token: Arc<Mutex<Option<(String, Instant)>>>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
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
    ean: Option<String>,
    /// Price is documented but not its shape; `amount` accepts the forms bol
    /// could plausibly send rather than betting on one.
    #[serde(default)]
    price: Option<serde_json::Value>,
    #[serde(default)]
    offer: Option<RawOffer>,
}

#[derive(Deserialize)]
struct RawOffer {
    #[serde(default)]
    price: Option<serde_json::Value>,
    #[serde(default, rename = "deliveryDescription")]
    delivery_description: Option<String>,
}

/// Pulls a number out of a plain number, a numeric string, or a small object
/// keyed `amount`/`value`.
fn amount(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.replace(',', ".").trim().parse().ok(),
        serde_json::Value::Object(map) => map
            .get("amount")
            .or_else(|| map.get("value"))
            .and_then(amount),
        _ => None,
    }
}

impl BolClient {
    pub fn new(
        http: reqwest::Client,
        client_id: String,
        client_secret: String,
        country: String,
        api_base: String,
        login_base: String,
    ) -> Self {
        Self {
            http,
            client_id,
            client_secret,
            country,
            api_base,
            login_base,
            token: Arc::new(Mutex::new(None)),
        }
    }

    async fn token(&self) -> Result<String, BolError> {
        if let Some((token, expires_at)) = self.token.lock().unwrap().clone() {
            if Instant::now() < expires_at {
                return Ok(token);
            }
        }
        let resp = self
            .http
            .post(format!("{}/token?grant_type=client_credentials", self.login_base))
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(BolError::Api { status: status.as_u16(), body: text });
        }
        let parsed: TokenResponse = serde_json::from_str(&text).map_err(|e| BolError::Decode {
            detail: format!("token response: {e}"),
        })?;
        let lifetime = Duration::from_secs(parsed.expires_in.unwrap_or(299));
        let expires_at = Instant::now() + lifetime.saturating_sub(TOKEN_MARGIN);
        *self.token.lock().unwrap() = Some((parsed.access_token.clone(), expires_at));
        Ok(parsed.access_token)
    }

    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<BolItem>, BolError> {
        let token = self.token().await?;
        let language = if self.country.eq_ignore_ascii_case("BE") { "nl-BE" } else { "nl" };
        let resp = self
            .http
            .get(format!("{}/marketing/catalog/v1/products/search", self.api_base))
            .bearer_auth(token)
            .header("Accept-Language", language)
            .header("Accept", "application/json")
            .query(&[
                ("search-term", query),
                ("country-code", &self.country),
                ("page-size", &limit.clamp(1, 50).to_string()),
                ("include-offer", "true"),
            ])
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(BolError::Api { status: status.as_u16(), body: text });
        }
        let parsed: SearchResponse = serde_json::from_str(&text).map_err(|e| BolError::Decode {
            detail: format!("search response: {e}; body: {}", text.chars().take(200).collect::<String>()),
        })?;
        Ok(parsed
            .results
            .into_iter()
            .filter_map(|raw| {
                let url = raw.url.filter(|u| !u.is_empty())?;
                let offer = raw.offer;
                let price = offer
                    .as_ref()
                    .and_then(|o| o.price.as_ref())
                    .or(raw.price.as_ref())
                    .and_then(amount)
                    .map(|p| format!("{p:.2} EUR"));
                Some(BolItem {
                    title: raw.title.unwrap_or_default(),
                    url,
                    ean: raw.ean,
                    price,
                    delivery: offer.and_then(|o| o.delivery_description),
                })
            })
            .take(limit)
            .collect())
    }
}

/// Live bol.com search. Present only when credentials are configured, so the
/// model never sees a tool that cannot work.
pub struct BolSearchTool {
    pub client: BolClient,
}

impl rig::tool::Tool for BolSearchTool {
    const NAME: &'static str = "search_bol";
    type Error = BolError;
    type Args = super::kagi::SearchArgs;
    type Output = Vec<BolItem>;

    fn description(&self) -> String {
        "Search bol.com's catalogue directly: live titles, prices, delivery \
         wording and product URLs for the Dutch/Belgian market. Prefer this \
         over a web search when looking for something on bol.com — the \
         results are current, so do not open them with fetch_page. Prices are \
         item-only: the delivery text gives timing, not shipping cost."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "product search term, in Dutch"}
            },
            "required": ["query"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        self.client.search(&args.query, 8).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> BolClient {
        BolClient::new(
            reqwest::Client::new(),
            "id".to_string(),
            "secret".to_string(),
            "NL".to_string(),
            server.uri(),
            server.uri(),
        )
    }

    async fn mount_token(server: &MockServer, expect: u64) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(query_param("grant_type", "client_credentials"))
            // base64("id:secret")
            .and(header("Authorization", "Basic aWQ6c2VjcmV0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "tok", "token_type": "Bearer", "expires_in": 299
            })))
            .expect(expect)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn searches_the_catalogue_and_reuses_the_token() {
        let server = MockServer::start().await;
        // one token for both searches: bol blocks IPs that re-fetch per call
        mount_token(&server, 1).await;
        Mock::given(method("GET"))
            .and(path("/marketing/catalog/v1/products/search"))
            .and(query_param("search-term", "wasmiddel"))
            .and(query_param("country-code", "NL"))
            .and(query_param("include-offer", "true"))
            .and(header("Authorization", "Bearer tok"))
            .and(header("Accept-Language", "nl"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": [
                {"ean": "8001841234567", "title": "Ariel Professional 5 L",
                 "url": "https://www.bol.com/nl/nl/p/ariel/9300000123456789/",
                 "offer": {"price": 23.95,
                           "deliveryDescription": "Voor 23:59 besteld, morgen in huis"}},
                {"title": "no url", "offer": {"price": 1.0}}
            ]})))
            .mount(&server)
            .await;

        let c = client(&server);
        let items = c.search("wasmiddel", 5).await.unwrap();
        assert_eq!(
            items,
            vec![BolItem {
                title: "Ariel Professional 5 L".into(),
                url: "https://www.bol.com/nl/nl/p/ariel/9300000123456789/".into(),
                ean: Some("8001841234567".into()),
                price: Some("23.95 EUR".into()),
                delivery: Some("Voor 23:59 besteld, morgen in huis".into()),
            }]
        );
        assert_eq!(c.search("wasmiddel", 5).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn price_shapes_are_read_defensively() {
        // The docs name the field but not its shape; all of these are prices.
        assert_eq!(amount(&json!(23.95)), Some(23.95));
        assert_eq!(amount(&json!("23,95")), Some(23.95));
        assert_eq!(amount(&json!({"amount": 23.95, "currency": "EUR"})), Some(23.95));
        assert_eq!(amount(&json!({"value": "23.95"})), Some(23.95));
        assert_eq!(amount(&json!(null)), None);
        assert_eq!(amount(&json!("free")), None);
    }

    #[tokio::test]
    async fn a_product_price_is_used_when_no_offer_is_returned() {
        let server = MockServer::start().await;
        mount_token(&server, 1).await;
        Mock::given(method("GET"))
            .and(path("/marketing/catalog/v1/products/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": [
                {"title": "T", "url": "https://bol.com/p", "price": {"amount": "9,99"}}
            ]})))
            .mount(&server)
            .await;

        let items = client(&server).search("q", 5).await.unwrap();
        assert_eq!(items[0].price, Some("9.99 EUR".into()));
        assert_eq!(items[0].delivery, None);
    }

    #[tokio::test]
    async fn bad_credentials_and_empty_results_surface_cleanly() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(401).set_body_string("invalid_client"))
            .mount(&server)
            .await;
        let err = client(&server).search("q", 5).await.unwrap_err();
        assert!(matches!(err, BolError::Api { status: 401, .. }), "got: {err}");

        let server = MockServer::start().await;
        mount_token(&server, 1).await;
        Mock::given(method("GET"))
            .and(path("/marketing/catalog/v1/products/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;
        assert!(client(&server).search("nothing", 5).await.unwrap().is_empty());
    }
}
