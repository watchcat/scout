use super::fetch::browser_get;
use serde::Deserialize;

/// The public JSON endpoint the marktplaats.nl frontend itself queries.
/// Unofficial: shape may change; failures degrade to the Kagi path upstream.
pub const MARKTPLAATS_BASE: &str = "https://www.marktplaats.nl";

#[derive(Debug, thiserror::Error)]
pub enum MarktplaatsError {
    #[error("marktplaats request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("marktplaats api error (status {status}): {body}")]
    Api { status: u16, body: String },
    #[error("marktplaats returned an unexpected response: {detail}")]
    Decode { detail: String },
}

/// A live listing from marktplaats.nl.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MarktplaatsItem {
    pub title: String,
    pub url: String,
    pub price: Option<String>, // "35.00 EUR (bidding from)" / "price negotiable"
    pub city: Option<String>,
}

/// `base_url` is injectable for tests.
#[derive(Clone)]
pub struct MarktplaatsClient {
    http: reqwest::Client,
    base_url: String,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    listings: Vec<RawListing>,
}

#[derive(Deserialize)]
struct RawListing {
    #[serde(default, rename = "itemId")]
    item_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default, rename = "priceInfo")]
    price_info: Option<RawPrice>,
    #[serde(default)]
    location: Option<RawLocation>,
}

#[derive(Deserialize)]
struct RawPrice {
    #[serde(default, rename = "priceCents")]
    price_cents: Option<i64>,
    #[serde(default, rename = "priceType")]
    price_type: Option<String>,
}

#[derive(Deserialize)]
struct RawLocation {
    #[serde(default, rename = "cityName")]
    city_name: Option<String>,
}

/// Human label for Marktplaats' price types (FIXED, MIN_BID, NOTK, ...).
fn price_label(cents: Option<i64>, price_type: Option<&str>) -> Option<String> {
    let eur = cents
        .filter(|c| *c > 0)
        .map(|c| format!("{:.2} EUR", c as f64 / 100.0));
    match (eur, price_type) {
        (Some(p), Some("MIN_BID")) => Some(format!("{p} (bidding from)")),
        (Some(p), Some("FAST_BID")) => Some(format!("{p} (bidding)")),
        (Some(p), _) => Some(p),
        (None, Some("FREE")) => Some("free".to_string()),
        (None, Some("NOTK")) => Some("price negotiable".to_string()),
        (None, Some("ON_REQUEST")) => Some("price on request".to_string()),
        (None, Some("SEE_DESCRIPTION")) => Some("see description".to_string()),
        (None, Some("RESERVED")) => Some("reserved".to_string()),
        _ => None,
    }
}

impl MarktplaatsClient {
    pub fn new(http: reqwest::Client, base_url: String) -> Self {
        Self { http, base_url }
    }

    pub async fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MarktplaatsItem>, MarktplaatsError> {
        let resp = browser_get(&self.http, format!("{}/lrp/api/search", self.base_url))
            .query(&[
                ("query", query),
                ("limit", &limit.to_string()),
                ("offset", "0"),
            ])
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            let body: String = text.chars().take(200).collect();
            return Err(MarktplaatsError::Api { status: status.as_u16(), body });
        }
        let parsed: SearchResponse =
            serde_json::from_str(&text).map_err(|e| MarktplaatsError::Decode {
                detail: format!("{e}; body: {}", text.chars().take(200).collect::<String>()),
            })?;
        Ok(parsed
            .listings
            .into_iter()
            .filter_map(|raw| {
                let item_id = raw.item_id?;
                Some(MarktplaatsItem {
                    title: raw.title.unwrap_or_default(),
                    url: format!("https://link.marktplaats.nl/{item_id}"),
                    price: raw.price_info.as_ref().and_then(|p| {
                        price_label(p.price_cents, p.price_type.as_deref())
                    }),
                    city: raw.location.and_then(|l| l.city_name),
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
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn price_labels_cover_the_marktplaats_price_types() {
        assert_eq!(price_label(Some(3500), Some("FIXED")), Some("35.00 EUR".into()));
        assert_eq!(
            price_label(Some(3500), Some("MIN_BID")),
            Some("35.00 EUR (bidding from)".into())
        );
        assert_eq!(price_label(None, Some("NOTK")), Some("price negotiable".into()));
        assert_eq!(price_label(Some(0), Some("FREE")), Some("free".into()));
        assert_eq!(price_label(None, None), None);
    }

    #[tokio::test]
    async fn search_maps_listings_to_link_urls() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/lrp/api/search"))
            .and(query_param("query", "usb hub"))
            .and(query_param("limit", "5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "totalResultCount": 2120,
                "listings": [
                    {"itemId": "m2424295168", "title": "SATECHI 4-in-1 USB-C hub",
                     "priceInfo": {"priceCents": 3500, "priceType": "MIN_BID"},
                     "location": {"cityName": "Utrecht"}},
                    {"itemId": "a1527336428", "title": "We Sing USB Hub",
                     "priceInfo": {"priceCents": 698, "priceType": "FIXED"}},
                    {"title": "no item id, skipped"}
                ]
            })))
            .mount(&server)
            .await;

        let items = MarktplaatsClient::new(reqwest::Client::new(), server.uri())
            .search("usb hub", 5)
            .await
            .unwrap();
        assert_eq!(
            items,
            vec![
                MarktplaatsItem {
                    title: "SATECHI 4-in-1 USB-C hub".into(),
                    url: "https://link.marktplaats.nl/m2424295168".into(),
                    price: Some("35.00 EUR (bidding from)".into()),
                    city: Some("Utrecht".into()),
                },
                MarktplaatsItem {
                    title: "We Sing USB Hub".into(),
                    url: "https://link.marktplaats.nl/a1527336428".into(),
                    price: Some("6.98 EUR".into()),
                    city: None,
                },
            ]
        );
    }

    #[tokio::test]
    async fn non_success_surfaces_as_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/lrp/api/search"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
            .mount(&server)
            .await;

        let err = MarktplaatsClient::new(reqwest::Client::new(), server.uri())
            .search("x", 5)
            .await
            .unwrap_err();
        assert!(matches!(err, MarktplaatsError::Api { status: 429, .. }), "got: {err}");
    }
}
