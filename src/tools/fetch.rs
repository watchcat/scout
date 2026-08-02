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
/// turn budget before it answers — so the tool enforces it too. Three
/// proved too tight once repeats stopped being charged: a comparison across
/// four shops needs four pages, and the fifth is headroom.
const MAX_OPENS: usize = 5;
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

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PageLink {
    pub url: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
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

/// Every `<script type="{mime}">` payload that parses. Two mime types matter:
/// `application/ld+json` for schema.org markup, and `application/json` for the
/// framework state blocks (`__NUXT_DATA__`, `__NEXT_DATA__`) that shops
/// without schema.org markup still ship. The two never collide as substrings.
fn script_json_blocks(html: &str, mime: &str) -> Vec<serde_json::Value> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(start) = lower[from..].find("<script").map(|i| i + from) {
        let Some(open_end) = lower[start..].find('>').map(|i| i + start + 1) else { break };
        let is_wanted = lower[start..open_end].contains(mime);
        let Some(close) = lower[open_end..].find("</script").map(|i| i + open_end) else { break };
        if is_wanted {
            if let Ok(value) = serde_json::from_str(html[open_end..close].trim()) {
                out.push(value);
            }
        }
        from = close + 1;
    }
    out
}

/// Corrects a price that is a hundred times the page's own OpenGraph
/// amount. That is the shape of a cents-in-euros slip, and it is worth a
/// dedicated check because it is the error that survives every other one:
/// 640 EUR for a bottle of cola reads as a real number, not a bug.
///
/// Only that exact factor is corrected. Any other disagreement is left
/// alone rather than guessed at.
fn with_og_correction(product: PageProduct, html: &str) -> PageProduct {
    let (Some(price), Some(og)) = (product.price.as_deref(), og_price(html)) else {
        return product;
    };
    let Some(shown) = price.split_whitespace().next().and_then(|p| p.parse::<f64>().ok()) else {
        return product;
    };
    if og > 0.0 && (shown / 100.0 - og).abs() < 0.005 {
        let currency = price.split_once(' ').map(|(_, c)| c.to_string());
        return PageProduct {
            price: Some(match currency {
                Some(currency) => format!("{og:.2} {currency}"),
                None => format!("{og:.2}"),
            }),
            ..product
        };
    }
    product
}

/// One product the markup describes, before deciding which is the page's.
struct Candidate {
    url: Option<String>,
    /// Whether the price was written with decimals. A bare integer is how a
    /// broken shop theme leaks cents: japanesetaste.nl publishes "6.48" for
    /// this bottle in one JSON-LD block and "648" in another.
    decimals: bool,
    product: PageProduct,
}

/// The page's OpenGraph product price, an independent statement of the same
/// number that shop themes get right far more often than their JSON-LD.
fn og_price(html: &str) -> Option<f64> {
    let lower = html.to_ascii_lowercase();
    let at = lower.find("property=\"product:price:amount\"")?;
    let open = lower[..at].rfind('<')?;
    let close = lower[at..].find('>')? + at;
    let tag = &html[open..close];
    let start = tag.to_ascii_lowercase().find("content=\"")? + "content=\"".len();
    let end = tag[start..].find('"')? + start;
    normalize_price(&tag[start..end])?.parse().ok()
}

/// Collects every node that carries an offer with a price, keeping the url
/// it claims — a bol.com page is one ProductGroup whose variants each have
/// their own url and price, and only one of them is the page you fetched.
fn collect_products(node: &serde_json::Value, out: &mut Vec<Candidate>) {
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
                let raw = text(offer.get("price")).unwrap_or_default();
                let price = normalize_price(&raw).map(|p| {
                    match text(offer.get("priceCurrency")) {
                        Some(currency) => format!("{p} {currency}"),
                        None => p,
                    }
                });
                out.push(Candidate {
                    url: text(map.get("url")),
                    decimals: raw.contains('.') || raw.contains(','),
                    product: PageProduct {
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
                });
            }
            map.values().for_each(|v| collect_products(v, out));
        }
        _ => {}
    }
}

/// The value a microdata property carries: `content`/`href` when the tag has
/// one (meta/link), otherwise the tag's own text.
fn microdata_values(html: &str, prop: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let needle = format!("itemprop=\"{prop}\"");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = lower[from..].find(&needle).map(|i| i + from) {
        from = at + needle.len();
        let Some(open) = lower[..at].rfind('<') else { continue };
        let Some(close) = lower[at..].find('>').map(|i| i + at) else { continue };
        let tag = &html[open..close];
        let attr = ["content=\"", "href=\""].iter().find_map(|a| {
            let start = tag.to_ascii_lowercase().find(a)? + a.len();
            let end = tag[start..].find('"')? + start;
            Some(tag[start..end].trim().to_string())
        });
        let value = attr.unwrap_or_else(|| {
            let text = &html[close + 1..];
            let end = text.find('<').unwrap_or(text.len());
            text[..end].trim().to_string()
        });
        if !value.is_empty() {
            out.push(value);
        }
    }
    out
}

/// Prices as written on a page: "13,80", "€ 13.80", "13.80 EUR".
fn normalize_price(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == ',' || *c == '.')
        .collect();
    let cleaned = cleaned.replace(',', ".");
    // "1.234.56" style thousands separators: keep the last dot as decimal.
    let value: f64 = match cleaned.matches('.').count() {
        0 | 1 => cleaned.parse().ok()?,
        _ => {
            let cut = cleaned.rfind('.')?;
            format!("{}.{}", cleaned[..cut].replace('.', ""), &cleaned[cut + 1..])
                .parse()
                .ok()?
        }
    };
    (value.is_finite() && value > 0.0).then(|| format!("{value:.2}"))
}

/// schema.org microdata, used by shops that never adopted JSON-LD.
///
/// Only a page carrying exactly one price is accepted: without a DOM there
/// is no way to tell which `itemprop="price"` belongs to the product and
/// which to a related-items strip, and a confident wrong price is the one
/// outcome worth avoiding.
fn microdata_product(html: &str) -> Option<PageProduct> {
    let prices = microdata_values(html, "price");
    let [price] = prices.as_slice() else { return None };
    let price = normalize_price(price)?;
    let currency = microdata_values(html, "pricecurrency")
        .first()
        .map(|c| c.trim().to_uppercase())
        .unwrap_or_else(|| "EUR".to_string());
    Some(PageProduct {
        name: microdata_values(html, "name").first().cloned(),
        price: Some(format!("{price} {currency}")),
        availability: microdata_values(html, "availability")
            .first()
            .and_then(|a| availability_word(a)),
        seller: None,
    })
}

/// Field names a shop's own state uses for the product's address, for the
/// amount, and for the object the amount may sit one level down in.
const URL_KEYS: [&str; 5] = ["url", "link", "slug", "path", "canonicalUrl"];
const PRICE_KEYS: [&str; 3] = ["price", "amount", "value"];
const PRICE_CONTAINER_KEYS: [&str; 3] = ["price", "offers", "pricing"];
const STOCK_KEYS: [&str; 4] = ["availability", "stock", "inStock", "isAvailable"];

/// Nesting allowed when walking a plain payload, and hops allowed along a
/// devalue reference chain. Application state is routinely cyclic once a
/// framework's reactivity layer has been through it, and devalue's index
/// table can encode those cycles, so both walks are capped rather than
/// trusted to terminate.
const MAX_JSON_DEPTH: usize = 64;
const MAX_REF_HOPS: usize = 16;

type JsonObject = serde_json::Map<String, serde_json::Value>;

/// A parsed `<script type="application/json">` payload in one of the two
/// shapes shops ship: plain nested JSON, or devalue — the flattened form Nuxt
/// emits, where the root array is a value table and every field inside a
/// container holds an index into that table instead of the value itself.
enum Payload<'a> {
    Plain,
    Devalue(&'a [serde_json::Value]),
}

impl<'a> Payload<'a> {
    /// devalue containers hold nothing but indices, so a root array whose
    /// objects carry only numbers — at least one of which lands on another
    /// container — is the flattened form; anything else is read as it stands.
    /// Guessing this wrong is expensive in both directions: read plain JSON as
    /// devalue and every price becomes a table lookup, read devalue as plain
    /// and the index 1299 is reported as a price.
    fn detect(root: &'a serde_json::Value) -> Self {
        let Some(table) = root.as_array() else { return Payload::Plain };
        let mut refs = 0usize;
        for value in table.iter().filter_map(|v| v.as_object()).flat_map(|o| o.values()) {
            // devalue writes undefined and holes as negative numbers, so only
            // the non-negative values are indices; a literal of any other type
            // means this is ordinary JSON.
            let Some(index) = value.as_u64() else {
                if !value.is_number() {
                    return Payload::Plain;
                }
                continue;
            };
            if table.get(index as usize).is_some_and(|v| v.is_object() || v.is_array()) {
                refs += 1;
            }
        }
        if refs > 0 {
            Payload::Devalue(table)
        } else {
            Payload::Plain
        }
    }

    /// What a field actually carries. A plain payload stores it inline; a
    /// devalue field stores an index, and the element it lands on may itself
    /// be a `["ShallowReactive", 1]` type wrapper standing for another index.
    fn resolve(&self, value: &'a serde_json::Value) -> Option<&'a serde_json::Value> {
        let Payload::Devalue(table) = self else { return Some(value) };
        // Exactly one index hop: what the table holds is the literal, even
        // when that literal is itself a number.
        let mut current = table.get(value.as_u64()? as usize)?;
        for _ in 0..MAX_REF_HOPS {
            match current.as_array().map(|items| items.as_slice()) {
                Some([serde_json::Value::String(_), inner]) => {
                    current = table.get(inner.as_u64()? as usize)?;
                }
                _ => return Some(current),
            }
        }
        None
    }

    /// Every object the payload contains. A devalue table already holds each
    /// container at its top level, so there is nothing to walk.
    fn objects(&self, root: &'a serde_json::Value) -> Vec<&'a JsonObject> {
        let mut out = Vec::new();
        match self {
            Payload::Devalue(table) => out.extend(table.iter().filter_map(|v| v.as_object())),
            Payload::Plain => collect_objects(root, 0, &mut out),
        }
        out
    }
}

fn collect_objects<'a>(node: &'a serde_json::Value, depth: usize, out: &mut Vec<&'a JsonObject>) {
    if depth > MAX_JSON_DEPTH {
        return;
    }
    match node {
        serde_json::Value::Object(map) => {
            out.push(map);
            map.values().for_each(|v| collect_objects(v, depth + 1, out));
        }
        serde_json::Value::Array(items) => {
            items.iter().for_each(|v| collect_objects(v, depth + 1, out));
        }
        _ => {}
    }
}

/// The first of these fields that resolves to a non-empty string.
fn string_field<'a>(obj: &'a JsonObject, payload: &Payload<'a>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        let text = payload.resolve(obj.get(*key)?)?.as_str()?.trim();
        (!text.is_empty()).then(|| text.to_string())
    })
}

fn currency_field<'a>(obj: &'a JsonObject, payload: &Payload<'a>) -> Option<String> {
    string_field(obj, payload, &["currency", "priceCurrency"]).map(|c| c.to_uppercase())
}

/// How much of the page's path this object claims as its own url, when it
/// claims the page at all. Same prefix rule as the JSON-LD matcher, since
/// shops shorten paths in their state as freely as in their markup — plus a
/// tail match for `slug`, which is never a path and so can never be a prefix.
fn matched_path<'a>(obj: &'a JsonObject, payload: &Payload<'a>, path: &str) -> Option<usize> {
    URL_KEYS
        .iter()
        .filter_map(|key| {
            let claimed = string_field(obj, payload, std::slice::from_ref(key))?;
            let claimed = claimed.split(['?', '#']).next()?;
            let claimed = match Url::parse(claimed) {
                Ok(url) => url.path().to_string(),
                Err(_) => claimed.to_string(),
            };
            let claimed = claimed.trim_end_matches('/').to_ascii_lowercase();
            if claimed.len() < 2 {
                return None;
            }
            let hit = if claimed.starts_with('/') {
                path.starts_with(&claimed) || claimed.starts_with(path)
            } else {
                path.ends_with(&format!("/{claimed}"))
            };
            hit.then_some(claimed.len())
        })
        .max()
}

fn as_scalar(value: &serde_json::Value) -> Option<&serde_json::Value> {
    matches!(value, serde_json::Value::Number(_) | serde_json::Value::String(_)).then_some(value)
}

/// The object's raw price value and the currency written beside it: either on
/// the object itself, or one level down in the object shops park it in
/// (`{"price": {"__typename": "Price", "price": 1299}}` is Jumbo's shape).
fn scalar_price<'a>(
    obj: &'a JsonObject,
    payload: &Payload<'a>,
) -> Option<(&'a serde_json::Value, Option<String>)> {
    let direct = PRICE_KEYS.iter().find_map(|key| as_scalar(payload.resolve(obj.get(*key)?)?));
    if let Some(value) = direct {
        return Some((value, currency_field(obj, payload)));
    }
    PRICE_CONTAINER_KEYS.iter().find_map(|key| {
        let nested = payload.resolve(obj.get(*key)?)?;
        let nested = match nested {
            // an offers list: the first entry that is an object
            serde_json::Value::Array(items) => {
                items.iter().find_map(|i| payload.resolve(i)?.as_object())
            }
            other => other.as_object(),
        }?;
        let value = PRICE_KEYS.iter().find_map(|key| as_scalar(payload.resolve(nested.get(*key)?)?))?;
        Some((value, currency_field(nested, payload).or_else(|| currency_field(obj, payload))))
    })
}

/// A payload's price in major units ("12.99"), or nothing when the payload
/// leaves it ambiguous.
///
/// Embedded state stores minor units as readily as major: Jumbo's price is the
/// integer 1299 for €12.99, and nothing in the JSON says which it is — €1299
/// is an equally well-formed reading, and reporting that would be a far worse
/// failure than reporting no price at all. A decimal separator settles it; a
/// bare integer is settled only by the page writing the amount out in its own
/// visible text, and when it does not, the price goes unreported.
fn payload_price(value: &serde_json::Value, text: &str) -> Option<String> {
    let integer = match value {
        serde_json::Value::Number(n) if n.is_f64() => return normalize_price(&n.to_string()),
        serde_json::Value::Number(n) => n.as_i64()?,
        serde_json::Value::String(s) if s.contains(['.', ',']) => return normalize_price(s),
        serde_json::Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    if integer <= 0 {
        return None;
    }
    let cents = format!("{:.2}", integer as f64 / 100.0);
    if text_writes_amount(text, &cents) {
        return Some(cents);
    }
    let major = format!("{integer}.00");
    text_writes_amount(text, &major).then_some(major)
}

/// Whether the page writes this amount out as a price — "12.99" or "12,99",
/// and not as a fragment of a longer number, so that "112,99" elsewhere on the
/// page cannot confirm 12.99.
fn text_writes_amount(text: &str, amount: &str) -> bool {
    let comma = amount.replace('.', ",");
    [amount, comma.as_str()].iter().any(|needle| {
        let mut from = 0;
        while let Some(at) = text[from..].find(*needle).map(|i| i + from) {
            let end = at + needle.len();
            let free = |c: Option<char>| !c.is_some_and(|c| c.is_ascii_digit() || c == ',' || c == '.');
            if free(text[..at].chars().next_back()) && free(text[end..].chars().next()) {
                return true;
            }
            from = end;
        }
        false
    })
}

/// Stock as embedded state spells it: a schema.org word, a screaming-snake
/// enum, or a boolean.
fn stock_word(value: &serde_json::Value) -> Option<&'static str> {
    let text = match value {
        serde_json::Value::Bool(b) => return Some(if *b { "in stock" } else { "out of stock" }),
        serde_json::Value::String(s) => s.to_ascii_uppercase(),
        _ => return None,
    };
    availability_word(&text).or_else(|| {
        // "UNAVAILABLE" contains "AVAILABLE", so the negatives go first.
        let says = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
        if says(&["OUT_OF_STOCK", "UNAVAILABLE", "SOLD_OUT", "FALSE"]) {
            Some("out of stock")
        } else if says(&["AVAILABLE", "IN_STOCK", "TRUE"]) {
            Some("in stock")
        } else {
            None
        }
    })
}

fn payload_availability<'a>(obj: &'a JsonObject, payload: &Payload<'a>) -> Option<&'static str> {
    STOCK_KEYS.iter().find_map(|key| {
        let value = payload.resolve(obj.get(*key)?)?;
        stock_word(value).or_else(|| {
            // Jumbo nests it: availability -> { isAvailable, availability }.
            let nested = value.as_object()?;
            STOCK_KEYS.iter().find_map(|key| stock_word(payload.resolve(nested.get(*key)?)?))
        })
    })
}

/// The page as a reader sees it, used to confirm a price the embedded state
/// left ambiguous.
fn visible_text(html: &str) -> String {
    let without_scripts = strip_tag_blocks(&strip_tag_blocks(html, "script"), "style");
    collapse_ws(&decode_entities(&strip_tags(&without_scripts)))
}

/// Shops that ship no schema.org markup still ship their own state: a
/// `<script type="application/json">` block holding the very product the page
/// was rendered from. Jumbo is the case that prompted this — its JSON-LD offer
/// carries no price at all, while its Nuxt payload has the product, its url
/// and its price.
///
/// Only an object whose own url matches the fetched page is read. A price
/// found loose in a payload is exactly the guess this extractor exists to
/// prevent, so an unmatched payload yields nothing.
fn embedded_json_product(html: &str, page: &Url) -> Option<PageProduct> {
    let path = page.path().trim_end_matches('/').to_ascii_lowercase();
    for root in script_json_blocks(html, "application/json") {
        let payload = Payload::detect(&root);
        let mut best: Option<(usize, &JsonObject)> = None;
        for obj in payload.objects(&root) {
            // A url alone is not enough: the Nuxt root object names the page's
            // own path too, and it is not a product.
            let Some(claimed) = matched_path(obj, &payload, &path) else { continue };
            if scalar_price(obj, &payload).is_none() {
                continue;
            }
            if best.as_ref().is_none_or(|(len, _)| claimed > *len) {
                best = Some((claimed, obj));
            }
        }
        let Some((_, obj)) = best else { continue };
        let Some((raw, currency)) = scalar_price(obj, &payload) else { continue };
        // Stripping a 600 KB page is not free, so it waits for a match.
        let text = visible_text(html);
        let product = PageProduct {
            name: string_field(obj, &payload, &["name", "title"]),
            price: payload_price(raw, &text)
                .map(|price| format!("{price} {}", currency.unwrap_or_else(|| "EUR".into()))),
            availability: payload_availability(obj, &payload),
            seller: string_field(obj, &payload, &["seller", "merchant"]),
        };
        if product.price.is_none() && product.name.is_none() && product.availability.is_none() {
            return None;
        }
        return Some(product);
    }
    None
}

/// The product this URL is actually about. Matching is on the url path
/// because shops shorten it in markup (no product id, no trailing slash), so
/// one has to be a prefix of the other; the longest such match wins.
fn extract_product(html: &str, page: &Url) -> Option<PageProduct> {
    let mut found = Vec::new();
    for block in script_json_blocks(html, "application/ld+json") {
        collect_products(&block, &mut found);
    }
    let path = page.path().trim_end_matches('/').to_ascii_lowercase();
    // Ranked by (price written with decimals, length of the url match): a
    // shop that describes one product twice, once at "6.48" and once at
    // "648", is telling us which of the two it means by how it wrote it.
    let mut best: Option<((bool, usize), PageProduct)> = None;
    for candidate in &found {
        // Relative urls are normal in schema.org and must be resolved
        // against the page — parsing them standalone fails, which silently
        // dropped the only correct block on a Shopify page.
        let Some(claimed) = candidate
            .url
            .as_deref()
            .and_then(|u| page.join(u).ok())
        else {
            continue;
        };
        let claimed = claimed.path().trim_end_matches('/').to_ascii_lowercase();
        let rank = (candidate.decimals, claimed.len());
        if claimed.len() > 1
            && (path.starts_with(&claimed) || claimed.starts_with(&path))
            && best.as_ref().is_none_or(|(seen, _)| rank > *seen)
        {
            best = Some((rank, candidate.product.clone()));
        }
    }
    best.map(|(_, p)| p)
        // A page describing exactly one product needs no disambiguation.
        .or_else(|| (found.len() == 1).then(|| found[0].product.clone()))
        .map(|p| with_og_correction(p, html))
        .or_else(|| microdata_product(html))
        // Last: shops with no schema.org markup at all, read from the JSON
        // application state they render the page from.
        .or_else(|| embedded_json_product(html, page))
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
    /// Headless-Chrome fallback, when a browser is installed. Only reached
    /// after plain HTTP has failed.
    pub renderer: Option<super::browser::Renderer>,
    opens: std::sync::atomic::AtomicUsize,
    /// Pages already read during this request. A model that re-opens one it
    /// has seen gets it back for free: without this, a repeat fetch of the
    /// same kruidvat page ate a third of the budget and left none for the
    /// Action listing the user was asking about.
    seen: std::sync::Mutex<std::collections::HashMap<String, PageContent>>,
}

impl FetchPageTool {
    pub fn new(http: reqwest::Client, renderer: Option<super::browser::Renderer>) -> Self {
        Self {
            http,
            renderer,
            opens: std::sync::atomic::AtomicUsize::new(0),
            seen: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Keeps a page for the rest of the request and hands back a copy.
    fn remember(&self, key: String, page: PageContent) -> PageContent {
        self.seen.lock().unwrap().insert(key, page.clone());
        page
    }
}

/// Whether a headless render is worth trying after a plain fetch.
///
/// 404/410 mean the page is gone — rendering cannot bring it back, and
/// pretending otherwise would undo the dead-link checks. A bot wall (403,
/// 429, 503) or a challenge page served with HTTP 200 is exactly what a real
/// browser is for.
fn worth_rendering(status: Option<u16>, body: &str) -> bool {
    match status {
        Some(404 | 410) => false,
        Some(403 | 429 | 503) | None => true,
        Some(s) if !(200..300).contains(&s) => true,
        // A success that carries no readable content: challenge or a shell
        // page whose body arrives by script.
        _ => super::browser::looks_unrendered(body),
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
        // A page already read this request is free: re-reading it cannot
        // tell us anything new, and charging for it costs the budget a page
        // that would have.
        let key = url.as_str().trim_end_matches('/').to_string();
        if let Some(cached) = self.seen.lock().unwrap().get(&key) {
            tracing::info!(url = %url, "already read this request; serving the page again");
            return Ok(cached.clone());
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
        let body = if status.is_success() { resp.text().await? } else { String::new() };

        // Plain HTTP could not read this page; a real browser sometimes can.
        if let Some(renderer) = self
            .renderer
            .as_ref()
            .filter(|_| worth_rendering(Some(status.as_u16()), &body))
        {
            tracing::info!(url = %url, status = status.as_u16(), "retrying with headless browser");
            match renderer.render(url.as_str()).await {
                Ok(html) if !super::browser::looks_unrendered(&html) => {
                    return Ok(self.remember(key, extract_page(&html, &url)));
                }
                Ok(_) => tracing::info!(url = %url, "render produced no usable page"),
                Err(e) => tracing::warn!(url = %url, error = %e, "render failed"),
            }
        }
        if !status.is_success() {
            return Err(FetchError::Invalid(format!("page returned HTTP {status}")));
        }
        Ok(self.remember(key, extract_page(&body, &url)))
    }
}

#[cfg(test)]
pub fn extract_page_for_test(html: &str, url: &str) -> Option<PageProduct> {
    extract_page(html, &Url::parse(url).unwrap()).product
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

        let tool = FetchPageTool::new(reqwest::Client::new(), None);
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
    fn a_shop_contradicting_itself_loses_to_the_price_it_wrote_with_decimals() {
        // japanesetaste.nl, verbatim in shape: two Product blocks for one
        // bottle. The correct one carries a relative url, the one leaking
        // Shopify's cents carries the absolute url that matches the page.
        // We reported 648 EUR for a 6.48 EUR bottle of cola.
        let html = r##"<html><head>
          <meta property="product:price:amount" content="6,48">
          <script type="application/ld+json">
          {"@type":"Product","name":"Coca-Cola Fiber 470ml",
           "url":"/products/coca-cola-fiber-470ml",
           "offers":{"@type":"Offer","priceCurrency":"EUR","price":"6.48",
                     "availability":"InStock","seller":{"name":"Japanese Taste"}}}
          </script>
          <script type="application/ld+json">
          {"@type":"Product","name":"Coca-Cola Fiber 470ml",
           "url":"https://shop.example/products/coca-cola-fiber-470ml",
           "offers":[{"@type":"Offer","price":"648","priceCurrency":"EUR",
                      "availability":"InStock"}]}
          </script></head><body>ok</body></html>"##;
        let url = Url::parse("https://shop.example/products/coca-cola-fiber-470ml").unwrap();

        let product = extract_page(html, &url).product.expect("product");
        assert_eq!(product.price.as_deref(), Some("6.48 EUR"));
        assert_eq!(product.seller.as_deref(), Some("Japanese Taste"));
    }

    #[test]
    fn a_cents_price_is_corrected_against_the_pages_own_og_amount() {
        // Same slip with only the broken block present: the OpenGraph
        // amount is an independent statement of the same number.
        let html = r##"<html><head>
          <meta property="product:price:amount" content="6,48">
          <script type="application/ld+json">
          {"@type":"Product","name":"Cola","url":"https://shop.example/p/cola",
           "offers":{"price":"648","priceCurrency":"EUR"}}
          </script></head><body>ok</body></html>"##;
        let product = extract_page(html, &Url::parse("https://shop.example/p/cola").unwrap())
            .product
            .expect("product");
        assert_eq!(product.price.as_deref(), Some("6.48 EUR"));

        // A disagreement that is NOT the cents factor is left alone: we do
        // not know which is right, and inventing an answer is the failure
        // this whole area exists to prevent.
        let other = html.replace(r#"content="6,48""#, r#"content="12,00""#);
        let product = extract_page(&other, &Url::parse("https://shop.example/p/cola").unwrap())
            .product
            .expect("product");
        assert_eq!(product.price.as_deref(), Some("648.00 EUR"));
    }

    #[test]
    fn rendering_is_reserved_for_pages_plain_http_cannot_read() {
        let real_page = "<html><body>".to_string() + &"content ".repeat(500) + "</body></html>";

        // Bot walls and challenges are what a browser is for.
        assert!(worth_rendering(Some(403), ""));
        assert!(worth_rendering(Some(429), ""));
        assert!(worth_rendering(Some(503), ""));
        assert!(worth_rendering(Some(200), "<html><title>Just a moment...</title></html>"));
        assert!(worth_rendering(Some(200), "<html><body>tiny shell</body></html>"));

        // A page that is simply gone stays gone: rendering it would undo the
        // dead-link checks.
        assert!(!worth_rendering(Some(404), ""));
        assert!(!worth_rendering(Some(410), ""));
        // A page we could read needs no browser.
        assert!(!worth_rendering(Some(200), &real_page));
    }


    #[test]
    fn microdata_is_used_when_there_is_no_json_ld() {
        let html = r#"<html><body>
          <div itemscope itemtype="https://schema.org/Product">
            <h1 itemprop="name">Vanish Oxi Action 1.5 kg</h1>
            <div itemprop="offers" itemscope itemtype="https://schema.org/Offer">
              <meta itemprop="price" content="13,80">
              <meta itemprop="priceCurrency" content="eur">
              <link itemprop="availability" href="https://schema.org/InStock">
            </div>
          </div>
          <div class="related">€ 9,99 € 24,50</div>
        </body></html>"#;
        let page = extract_page(html, &Url::parse("https://shop.example/p/1").unwrap());
        let product = page.product.expect("microdata describes the product");
        assert_eq!(product.price.as_deref(), Some("13.80 EUR"));
        assert_eq!(product.name.as_deref(), Some("Vanish Oxi Action 1.5 kg"));
        assert_eq!(product.availability, Some("in stock"));
    }

    #[test]
    fn microdata_with_several_prices_is_refused() {
        // A related-items strip carrying its own offers: no way to tell which
        // price is the product's, so the page reports none.
        let html = r#"<html><body>
          <span itemprop="price">13,80</span>
          <span itemprop="price">9,99</span>
        </body></html>"#;
        let page = extract_page(html, &Url::parse("https://shop.example/p/1").unwrap());
        assert_eq!(page.product, None);
    }

    #[test]
    fn written_prices_are_normalised() {
        assert_eq!(normalize_price("13,80").as_deref(), Some("13.80"));
        assert_eq!(normalize_price("€ 13.80").as_deref(), Some("13.80"));
        assert_eq!(normalize_price("1.234,56").as_deref(), Some("1234.56"));
        assert_eq!(normalize_price("9").as_deref(), Some("9.00"));
        assert_eq!(normalize_price("gratis"), None);
        assert_eq!(normalize_price("0,00"), None);
    }

    #[test]
    fn a_single_product_page_needs_no_url_match() {
        let html = r#"<html><body><script type="application/ld+json">
          {"@type":"Product","name":"Widget","offers":{"price":9.5,"priceCurrency":"EUR",
           "availability":"OutOfStock"}}
          </script></body></html>"#;
        let page = extract_page(html, &Url::parse("https://shop.example/anything").unwrap());
        let product = page.product.unwrap();
        // normalised to cents like every other price the model sees
        assert_eq!(product.price.as_deref(), Some("9.50 EUR"));
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

    /// The shape of Jumbo's `__NUXT_DATA__`, shrunk: a devalue value table
    /// where every field inside a container is an index into the table, the
    /// product's price is the integer 1299 for €12.99, and `["ShallowReactive",
    /// 1]` wrappers stand in front of the containers they wrap.
    fn nuxt_page(visible: &str) -> String {
        format!(
            r#"<html><body>
          <script type="application/json" id="__NUXT_DATA__">
          [["ShallowReactive",1],
           {{"data":2,"path":6,"serverRendered":10}},
           {{"product":3}},
           {{"title":4,"link":6,"canonicalUrl":13,"price":7,"availability":9}},
           "Vanish Oxi Action Poeder Kleur 720g",
           "Price",
           "/producten/vanish-oxi-action-poeder-kleur-720g-749765STK",
           {{"__typename":5,"price":8,"promoPrice":11}},
           1299,
           {{"__typename":5,"isAvailable":10,"availability":12}},
           true,
           null,
           "AVAILABLE",
           "https://www.jumbo.com/producten/vanish-oxi-action-poeder-kleur-720g-749765STK"]
          </script>
          <div class="price">{visible}</div>
          </body></html>"#
        )
    }

    fn jumbo_url() -> Url {
        Url::parse("https://www.jumbo.com/producten/vanish-oxi-action-poeder-kleur-720g-749765STK")
            .unwrap()
    }

    #[test]
    fn devalue_state_gives_the_price_the_page_confirms() {
        // No JSON-LD price and no microdata anywhere on the page: without the
        // payload there is nothing to report but the visible text.
        let html = nuxt_page("Prijs: &euro; 12,99 per stuk");
        let page = extract_page(&html, &jumbo_url());
        let product = page.product.expect("the Nuxt payload states the product");
        assert_eq!(product.price.as_deref(), Some("12.99 EUR"));
        assert_eq!(product.name.as_deref(), Some("Vanish Oxi Action Poeder Kleur 720g"));
        assert_eq!(product.availability, Some("in stock"));
        assert_eq!(page.availability, Some("in stock"));
    }

    #[test]
    fn an_integer_price_the_page_never_writes_out_is_refused() {
        // 1299 is €12.99 or €1299 and the payload does not say which. With no
        // written amount to settle it, the product is reported without a
        // price rather than with a possibly hundredfold wrong one.
        let html = nuxt_page("Prijs op aanvraag");
        let page = extract_page(&html, &jumbo_url());
        let product = page.product.expect("name and stock are still known");
        assert_eq!(product.price, None);
        assert_eq!(product.availability, Some("in stock"));

        // A longer number containing the amount does not count as confirmation.
        assert!(!text_writes_amount("Nu 112,99 in plaats van 129,95", "12.99"));
        // The integer reading is taken when that is what the page writes.
        assert_eq!(
            payload_price(&serde_json::json!(1299), "Laptop 1299,00 euro").as_deref(),
            Some("1299.00")
        );
    }

    #[test]
    fn plain_state_is_read_without_index_resolution() {
        // Next.js ships the state as ordinary nested JSON: the fields hold
        // their values, not indices into a table.
        let html = r#"<html><body>
          <script type="application/json" id="__NEXT_DATA__">
          {"props":{"pageProps":{"product":{
             "name":"Vanish Oxi Action 1.5 kg",
             "url":"https://www.bol.com/nl/nl/p/vanish-oxi-action-1-5-kg/",
             "price":"13.80","currency":"eur","inStock":true}}}}
          </script>
          <div>€ 9,99 € 24,50</div>
          </body></html>"#;
        let url = Url::parse("https://www.bol.com/nl/nl/p/vanish-oxi-action-1-5-kg/9300000087803700/")
            .unwrap();
        let product = extract_page(html, &url).product.expect("the payload states the product");
        assert_eq!(product.price.as_deref(), Some("13.80 EUR"));
        assert_eq!(product.name.as_deref(), Some("Vanish Oxi Action 1.5 kg"));
        assert_eq!(product.availability, Some("in stock"));
    }

    #[test]
    fn a_payload_matching_no_url_reports_nothing() {
        // The payload describes a product, the page is a different one: the
        // price on offer belongs to neither, so none is reported.
        let html = nuxt_page("&euro; 12,99");
        let page = extract_page(&html, &Url::parse("https://www.jumbo.com/producten/ariel").unwrap());
        assert_eq!(page.product, None);
    }

    #[test]
    fn a_cyclic_payload_terminates() {
        // devalue's table can reference itself; a wrapper chain that never
        // reaches a value must not hang or recurse away the stack.
        let html = r#"<html><body>
          <script type="application/json">
          [["ShallowReactive",1],{"data":0,"link":4,"price":2},
           ["Reactive",3],["Reactive",2],"/p/widget"]
          </script>
          <div>€ 12,99</div></body></html>"#;
        let page = extract_page(html, &Url::parse("https://shop.example/p/widget").unwrap());
        assert_eq!(page.product, None);
    }

    #[tokio::test]
    async fn re_reading_a_page_is_free() {
        // Observed in production: two of three opens went on the same
        // kruidvat page, so the Action listing the user asked about was
        // refused by our own budget and never priced.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/p/1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("<html><body>the first page</body></html>"),
            )
            // fetched once, however often the model asks for it
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/p/other"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html><body>ok</body></html>"))
            .mount(&server)
            .await;

        let tool = FetchPageTool::new(reqwest::Client::new(), None);
        let first = format!("{}/p/1", server.uri());
        for _ in 0..3 {
            let page = tool.call(FetchArgs { url: first.clone() }).await.unwrap();
            assert!(page.text.contains("the first page"));
        }
        // Those repeats cost no budget, so two further pages still open.
        for i in 0..2 {
            let url = format!("{}/p/other?n={i}", server.uri());
            assert!(tool.call(FetchArgs { url }).await.is_ok(), "open {i} should be allowed");
        }
    }

    #[tokio::test]
    async fn page_opens_are_capped_per_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html><body>ok</body></html>"))
            .mount(&server)
            .await;

        let tool = FetchPageTool::new(reqwest::Client::new(), None);
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
        let next = FetchPageTool::new(reqwest::Client::new(), None);
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

        let tool = FetchPageTool::new(reqwest::Client::new(), None);
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
