use reqwest::Url;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Headers for all page requests (fetch_page and link verification). A real
/// browser UA matters: e.g. Amazon serves full product pages to it but a
/// 503 bot-wall to bot-styled UAs — this is a personal assistant fetching a
/// handful of public pages, not bulk scraping.
pub(crate) const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// GET with browser-like headers; shared by fetch_page and link probing.
pub(crate) fn browser_get(http: &reqwest::Client, url: impl reqwest::IntoUrl) -> reqwest::RequestBuilder {
    http.get(url)
        .header("User-Agent", BROWSER_USER_AGENT)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "en-US,en;q=0.9,nl;q=0.8")
}

/// Readable-text cap: enough for a product/listing page's useful content
/// without flooding the model context.
const MAX_TEXT_CHARS: usize = 6000;
/// Page opens allowed per request. The preamble asks for at most this many,
/// but a model chasing one more listing will happily open eight and burn the
/// turn budget before it answers — so the tool enforces it too.
const MAX_OPENS: usize = 3;
const MAX_LINKS: usize = 30;
const MAX_LINK_TEXT_CHARS: usize = 120;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("fetch failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Deserialize)]
pub struct FetchArgs {
    pub url: String,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct PageLink {
    pub url: String,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct PageContent {
    pub text: String,
    pub truncated: bool,
    pub links: Vec<PageLink>,
    /// What the page's structured markup says about stock, when it says
    /// anything. `None` means unknown, not available.
    pub availability: Option<&'static str>,
    /// The page's own data for the product at this exact URL. Authoritative:
    /// the visible text of a shop page carries prices for carousels, other
    /// sellers and unrelated pack sizes, and nothing distinguishes them.
    pub product: Option<PageProduct>,
}

/// A product as the page's schema.org markup describes it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PageProduct {
    pub name: Option<String>,
    /// Formatted like every other price the model sees: "13.80 EUR".
    pub price: Option<String>,
    pub availability: Option<&'static str>,
    pub seller: Option<String>,
}

/// schema.org `availability` in either form: "InStock" or the full
/// "https://schema.org/InStock".
fn availability_word(value: &str) -> Option<&'static str> {
    let value = value.rsplit('/').next().unwrap_or(value).to_ascii_lowercase();
    match value.as_str() {
        "instock" | "limitedavailability" | "onlineonly" => Some("in stock"),
        "outofstock" | "soldout" | "discontinued" => Some("out of stock"),
        "preorder" | "backorder" => Some("on backorder"),
        _ => None,
    }
}

/// Every `<script type="application/ld+json">` payload that parses.
fn json_ld_blocks(html: &str) -> Vec<serde_json::Value> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(start) = lower[from..].find("<script").map(|i| i + from) {
        let Some(open_end) = lower[start..].find('>').map(|i| i + start + 1) else { break };
        let is_ld = lower[start..open_end].contains("application/ld+json");
        let Some(close) = lower[open_end..].find("</script").map(|i| i + open_end) else { break };
        if is_ld {
            if let Ok(value) = serde_json::from_str(html[open_end..close].trim()) {
                out.push(value);
            }
        }
        from = close + 1;
    }
    out
}

/// Collects every node that carries an offer with a price, keeping the url
/// it claims — a bol.com page is one ProductGroup whose variants each have
/// their own url and price, and only one of them is the page you fetched.
fn collect_products(node: &serde_json::Value, out: &mut Vec<(Option<String>, PageProduct)>) {
    match node {
        serde_json::Value::Array(items) => items.iter().for_each(|i| collect_products(i, out)),
        serde_json::Value::Object(map) => {
            let offer = match map.get("offers") {
                Some(serde_json::Value::Array(offers)) => {
                    offers.iter().find(|o| o.get("price").is_some())
                }
                Some(value @ serde_json::Value::Object(_)) if value.get("price").is_some() => {
                    Some(value)
                }
                _ => None,
            };
            if let Some(offer) = offer {
                let text = |v: Option<&serde_json::Value>| match v {
                    Some(serde_json::Value::String(s)) => Some(s.trim().to_string()),
                    Some(serde_json::Value::Number(n)) => Some(n.to_string()),
                    _ => None,
                };
                let price = text(offer.get("price")).map(|p| {
                    match text(offer.get("priceCurrency")) {
                        Some(currency) => format!("{p} {currency}"),
                        None => p,
                    }
                });
                out.push((
                    text(map.get("url")),
                    PageProduct {
                        name: text(map.get("name")),
                        price,
                        availability: text(offer.get("availability"))
                            .as_deref()
                            .and_then(availability_word),
                        seller: offer
                            .get("seller")
                            .and_then(|s| s.get("name"))
                            .and_then(|n| n.as_str())
                            .map(|n| n.to_string()),
                    },
                ));
            }
            map.values().for_each(|v| collect_products(v, out));
        }
        _ => {}
    }
}

/// The product this URL is actually about. Matching is on the url path
/// because shops shorten it in markup (no product id, no trailing slash), so
/// one has to be a prefix of the other; the longest such match wins.
fn extract_product(html: &str, page: &Url) -> Option<PageProduct> {
    let mut found = Vec::new();
    for block in json_ld_blocks(html) {
        collect_products(&block, &mut found);
    }
    let path = page.path().trim_end_matches('/').to_ascii_lowercase();
    let mut best: Option<(usize, PageProduct)> = None;
    for (url, product) in &found {
        let Some(claimed) = url.as_deref().and_then(|u| Url::parse(u).ok()) else { continue };
        let claimed = claimed.path().trim_end_matches('/').to_ascii_lowercase();
        if claimed.len() > 1
            && (path.starts_with(&claimed) || claimed.starts_with(&path))
            && best.as_ref().is_none_or(|(len, _)| claimed.len() > *len)
        {
            best = Some((claimed.len(), product.clone()));
        }
    }
    best.map(|(_, p)| p)
        // A page describing exactly one product needs no disambiguation.
        .or_else(|| (found.len() == 1).then(|| found[0].1.clone()))
}

/// Stock status from schema.org markup, which is the only trustworthy signal:
/// shops answer HTTP 200 for a product they cannot sell, and the visible text
/// is useless — a bol.com page for an unavailable item carries "Niet
/// leverbaar" once and seven "In winkelwagen" from its recommendations
/// carousel. Read from the raw HTML, since the markup lives in a JSON-LD
/// <script> block that text extraction throws away.
///
/// Markup that says both (a page listing several offers) is treated as
/// unknown rather than guessed at.
fn availability(html: &str) -> Option<&'static str> {
    let html = html.to_ascii_lowercase();
    let says = |needles: &[&str]| needles.iter().any(|n| html.contains(n));
    let out = says(&[
        "\"outofstock\"",
        "schema.org/outofstock",
        "\"soldout\"",
        "schema.org/soldout",
        "\"discontinued\"",
        "schema.org/discontinued",
    ]);
    let available = says(&[
        "\"instock\"",
        "schema.org/instock",
        "\"limitedavailability\"",
        "schema.org/limitedavailability",
    ]);
    match (out, available) {
        (true, false) => Some("out of stock"),
        (false, true) => Some("in stock"),
        _ => None,
    }
}

/// Plain-HTTP page fetcher: readable text + links, so the agent can open a
/// retailer listing page and pull out direct product URLs and prices.
/// Built per request, so `opens` counts this request's page opens.
pub struct FetchPageTool {
    pub http: reqwest::Client,
    opens: std::sync::atomic::AtomicUsize,
}

impl FetchPageTool {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http, opens: std::sync::atomic::AtomicUsize::new(0) }
    }
}

impl Tool for FetchPageTool {
    const NAME: &'static str = "fetch_page";
    type Error = FetchError;
    type Args = FetchArgs;
    type Output = PageContent;

    fn description(&self) -> String {
        "Fetch a web page and return its readable text, its links, and what \
         its markup says about stock ('out of stock' / 'in stock' / null when \
         the page does not say). Use it to open a retailer listing/product \
         page and extract direct product links and prices."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "http(s) URL of the page to fetch"}
            },
            "required": ["url"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let url = Url::parse(&args.url)
            .map_err(|e| FetchError::Invalid(format!("invalid url: {e}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(FetchError::Invalid(format!(
                "unsupported url scheme: {}",
                url.scheme()
            )));
        }
        // Counted only once the url is worth opening, so a malformed one
        // costs the model a turn but not part of its page budget.
        if self.opens.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= MAX_OPENS {
            return Err(FetchError::Invalid(format!(
                "page budget spent ({MAX_OPENS} opens per request) — answer with what you \
                 already have instead of opening more pages"
            )));
        }
        let resp = browser_get(&self.http, url.clone()).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(FetchError::Invalid(format!("page returned HTTP {status}")));
        }
        let html = resp.text().await?;
        Ok(extract_page(&html, &url))
    }
}

fn extract_page(html: &str, base: &Url) -> PageContent {
    let product = extract_product(html, base);
    // The product's own status beats a page-wide guess when both exist.
    let availability = product
        .as_ref()
        .and_then(|p| p.availability)
        .or_else(|| availability(html));
    let without_scripts = strip_tag_blocks(&strip_tag_blocks(html, "script"), "style");
    let links = extract_links(&without_scripts, base);
    let full_text = collapse_ws(&decode_entities(&strip_tags(&without_scripts)));
    let truncated = full_text.chars().count() > MAX_TEXT_CHARS;
    let text = if truncated {
        full_text.chars().take(MAX_TEXT_CHARS).collect()
    } else {
        full_text
    };
    PageContent { text, truncated, links, availability, product }
}

/// Remove `<tag ...>...</tag>` blocks wholesale (for script/style, whose
/// bodies are not content). ASCII-lowercased shadow keeps byte offsets valid.
fn strip_tag_blocks(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(html.len());
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find(&open) {
        let start = pos + rel;
        out.push_str(&html[pos..start]);
        match lower[start..].find(&close) {
            Some(rel_end) => pos = start + rel_end + close.len(),
            None => {
                pos = html.len();
                break;
            }
        }
    }
    out.push_str(&html[pos..]);
    out
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Decode the common entities; `&amp;` last so `&amp;lt;` becomes `&lt;`,
/// not `<`.
fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn extract_links(html: &str, base: &Url) -> Vec<PageLink> {
    let lower = html.to_ascii_lowercase();
    let mut links = Vec::new();
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find("<a") {
        let a_start = pos + rel;
        // Guard against <article>, <abbr>, ...: "<a" must end the tag name.
        match lower.as_bytes().get(a_start + 2) {
            Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r') | Some(b'>') => {}
            _ => {
                pos = a_start + 2;
                continue;
            }
        }
        let Some(tag_end_rel) = lower[a_start..].find('>') else {
            break;
        };
        let tag_end = a_start + tag_end_rel;
        let href = find_attr(&html[a_start..tag_end], "href");
        let Some(close_rel) = lower[tag_end..].find("</a>") else {
            pos = tag_end + 1;
            continue;
        };
        let close = tag_end + close_rel;
        let text = collapse_ws(&decode_entities(&strip_tags(&html[tag_end + 1..close])));
        pos = close + "</a>".len();

        let Some(href) = href else { continue };
        let href = decode_entities(href.trim());
        if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
            continue;
        }
        let Ok(abs) = base.join(&href) else { continue };
        if !matches!(abs.scheme(), "http" | "https") || text.is_empty() {
            continue;
        }
        links.push(PageLink {
            url: abs.to_string(),
            text: text.chars().take(MAX_LINK_TEXT_CHARS).collect(),
        });
        if links.len() >= MAX_LINKS {
            break;
        }
    }
    links
}

fn find_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let lower = tag.to_ascii_lowercase();
    let idx = lower.find(&format!("{name}="))?;
    let rest = &tag[idx + name.len() + 1..];
    let mut chars = rest.chars();
    match chars.next()? {
        q @ ('"' | '\'') => rest[1..].split(q).next(),
        _ => rest.split([' ', '\t', '\n', '\r', '>']).next(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn base() -> Url {
        Url::parse("https://shop.example/s/?q=widget").unwrap()
    }

    #[test]
    fn extracts_text_without_scripts_and_styles() {
        let html = r#"<html><head><style>.x{color:red}</style>
            <script>var a = "<b>ignored</b>";</script></head>
            <body><h1>Alfa AWUS036ACHM</h1><p>Price: &euro;class &#39;A&#39; &amp; 49.99</p></body></html>"#;
        let page = extract_page(html, &base());
        assert!(page.text.contains("Alfa AWUS036ACHM"));
        assert!(page.text.contains("'A' & 49.99"));
        assert!(!page.text.contains("ignored"));
        assert!(!page.text.contains("color:red"));
        assert!(!page.truncated);
    }

    #[test]
    fn extracts_and_absolutizes_links_skipping_junk() {
        let html = r##"<body>
            <a href="/nl/p/alfa-awus036achm/93000123/">Alfa AWUS036ACHM adapter</a>
            <a href="https://other.example/full">Full link</a>
            <a href="#reviews">Reviews</a>
            <a href="javascript:void(0)">JS</a>
            <a href="/empty-text"><img src="x.png"></a>
            <article>not a link</article>
            </body>"##;
        let page = extract_page(html, &base());
        assert_eq!(
            page.links,
            vec![
                PageLink {
                    url: "https://shop.example/nl/p/alfa-awus036achm/93000123/".into(),
                    text: "Alfa AWUS036ACHM adapter".into()
                },
                PageLink {
                    url: "https://other.example/full".into(),
                    text: "Full link".into()
                },
            ]
        );
    }

    #[test]
    fn long_text_is_truncated_with_flag() {
        let html = format!("<body>{}</body>", "word ".repeat(3000));
        let page = extract_page(&html, &base());
        assert!(page.truncated);
        assert_eq!(page.text.chars().count(), MAX_TEXT_CHARS);
    }

    #[test]
    fn link_cap_respected() {
        let many: String = (0..50)
            .map(|i| format!("<a href=\"/p/{i}\">Product {i}</a>"))
            .collect();
        let page = extract_page(&format!("<body>{many}</body>"), &base());
        assert_eq!(page.links.len(), MAX_LINKS);
    }

    #[tokio::test]
    async fn tool_fetches_and_extracts() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/p/1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<html><body><h1>Widget</h1><a href="/p/2">Other widget</a></body></html>"#,
            ))
            .mount(&server)
            .await;

        let tool = FetchPageTool::new(reqwest::Client::new());
        let page = tool
            .call(FetchArgs { url: format!("{}/p/1", server.uri()) })
            .await
            .unwrap();
        assert!(page.text.contains("Widget"));
        assert_eq!(page.links.len(), 1);
        assert!(page.links[0].url.ends_with("/p/2"));
    }


    #[test]
    fn the_price_comes_from_the_variant_matching_the_url() {
        // The real bol.com page a user sent us: one ProductGroup, two
        // variants with their own urls and prices, and ~40 unrelated euro
        // amounts in the visible text from carousels and other sellers. We
        // reported one of those instead of the 13.80 the page states.
        let html = r##"<html><body>
          <script type="application/ld+json">
          {"@type":"ProductGroup","name":"Vanish","hasVariant":[
            {"@type":"Product","name":"Vanish Oxi Action 1.5 kg",
             "url":"https://www.bol.com/nl/nl/p/vanish-oxi-action-poeder-vlekverwijderaar-voor-gekleurde-was-1-5-kg",
             "offers":{"@type":"Offer","price":"13.80","priceCurrency":"EUR",
                       "availability":"https://schema.org/InStock",
                       "seller":{"@type":"Organization","name":"MYSCO"}}},
            {"@type":"Product","name":"Vanish Oxi Action 10 x 1.5 kg",
             "url":"https://www.bol.com/nl/nl/p/vanish-oxi-action-veilige-kleur-poeder-zonder-bleek",
             "offers":{"@type":"Offer","price":"55.99","priceCurrency":"EUR",
                       "availability":"InStock"}}]}
          </script>
          <div class="carousel">€ 14,99 € 11,99 € 2,99 € 12,99</div>
          </body></html>"##;
        let url = Url::parse(
            "https://www.bol.com/nl/nl/p/vanish-oxi-action-poeder-vlekverwijderaar-voor-gekleurde-was-1-5-kg/9300000087803700/",
        )
        .unwrap();

        let page = extract_page(html, &url);
        let product = page.product.expect("the page states its own product");
        assert_eq!(product.price.as_deref(), Some("13.80 EUR"));
        assert_eq!(product.name.as_deref(), Some("Vanish Oxi Action 1.5 kg"));
        assert_eq!(product.seller.as_deref(), Some("MYSCO"));
        assert_eq!(product.availability, Some("in stock"));
        // the product's own status also settles the page-level field
        assert_eq!(page.availability, Some("in stock"));
    }

    #[test]
    fn a_single_product_page_needs_no_url_match() {
        let html = r#"<html><body><script type="application/ld+json">
          {"@type":"Product","name":"Widget","offers":{"price":9.5,"priceCurrency":"EUR",
           "availability":"OutOfStock"}}
          </script></body></html>"#;
        let page = extract_page(html, &Url::parse("https://shop.example/anything").unwrap());
        let product = page.product.unwrap();
        assert_eq!(product.price.as_deref(), Some("9.5 EUR"));
        assert_eq!(page.availability, Some("out of stock"));
    }

    #[test]
    fn pages_without_product_markup_report_nothing_rather_than_guessing() {
        let html = "<html><body><p>Great deal: € 9,99 today only</p></body></html>";
        let page = extract_page(html, &Url::parse("https://shop.example/p").unwrap());
        assert_eq!(page.product, None);
        assert_eq!(page.availability, None);
        // …and a variant list that matches no url leaves the price unclaimed
        let mismatched = r#"<html><body><script type="application/ld+json">
          {"@type":"ProductGroup","hasVariant":[
            {"url":"https://shop.example/other-a","offers":{"price":"1.00","priceCurrency":"EUR"}},
            {"url":"https://shop.example/other-b","offers":{"price":"2.00","priceCurrency":"EUR"}}]}
          </script></body></html>"#;
        let page = extract_page(mismatched, &Url::parse("https://shop.example/p").unwrap());
        assert_eq!(page.product, None);
    }

    #[test]
    fn availability_comes_from_markup_not_from_prose() {
        // Shape of the real bol.com page that prompted this: JSON-LD says the
        // product is gone, while the visible text is dominated by the
        // recommendations carousel saying the opposite.
        let bol = r##"<html><body>
            <script type="application/ld+json">{"@type":"Product","name":"Ariel Professional",
            "offers":{"@type":"Offer","availability":"OutOfStock"},"@context":"https://schema.org/"}</script>
            <p>Niet leverbaar</p>
            <div class="carousel">In winkelwagen<span>Op voorraad</span>In winkelwagen</div>
            </body></html>"##;
        let page = extract_page(bol, &Url::parse("https://www.bol.com/nl/nl/p/x/123/").unwrap());
        assert_eq!(page.availability, Some("out of stock"));
        // the JSON-LD itself must not leak into the readable text
        assert!(!page.text.contains("OutOfStock"), "got: {}", page.text);

        let in_stock = r#"<html><body><link itemprop="availability" href="https://schema.org/InStock"/></body></html>"#;
        assert_eq!(availability(in_stock), Some("in stock"));

        // A page carrying both (several offers) is unknown, not a guess.
        assert_eq!(availability(r#"{"availability":"OutOfStock"}{"availability":"InStock"}"#), None);
        // Dutch prose alone proves nothing — that is what the carousel showed.
        assert_eq!(availability("<p>Niet leverbaar, uitverkocht</p>"), None);
    }

    #[tokio::test]
    async fn page_opens_are_capped_per_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html><body>ok</body></html>"))
            .mount(&server)
            .await;

        let tool = FetchPageTool::new(reqwest::Client::new());
        for i in 0..MAX_OPENS {
            let page = tool.call(FetchArgs { url: format!("{}/p/{i}", server.uri()) }).await;
            assert!(page.is_ok(), "open {i} should be allowed");
        }
        let err = tool
            .call(FetchArgs { url: format!("{}/p/extra", server.uri()) })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("page budget spent"), "got: {err}");

        // A fresh tool (i.e. the next request) starts with a full budget.
        let next = FetchPageTool::new(reqwest::Client::new());
        assert!(next.call(FetchArgs { url: format!("{}/p/1", server.uri()) }).await.is_ok());
    }

    #[tokio::test]
    async fn tool_reports_http_errors_and_bad_urls() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let tool = FetchPageTool::new(reqwest::Client::new());
        let err = tool
            .call(FetchArgs { url: format!("{}/gone", server.uri()) })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"), "got: {err}");

        let err = tool
            .call(FetchArgs { url: "ftp://x".into() })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("scheme"), "got: {err}");
    }
}
