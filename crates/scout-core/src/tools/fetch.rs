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

pub(crate) const BROWSER_ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
pub(crate) const BROWSER_ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9,nl;q=0.8";

/// GET with browser-like headers; shared by fetch_page and link probing.
pub(crate) fn browser_get(http: &reqwest::Client, url: impl reqwest::IntoUrl) -> reqwest::RequestBuilder {
    http.get(url)
        .header("User-Agent", BROWSER_USER_AGENT)
        .header("Accept", BROWSER_ACCEPT)
        .header("Accept-Language", BROWSER_ACCEPT_LANGUAGE)
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
    /// Public internet only — see `net_guard`. Never the integrations'
    /// shared client: every URL this tool opens was chosen by the model.
    pub web: crate::net_guard::OpenWeb,
    /// Headless-Chrome fallback, when a browser is installed. Only reached
    /// after plain HTTP has failed.
    pub renderer: Option<super::browser::Renderer>,
    opens: std::sync::atomic::AtomicUsize,
    /// Pages already read during this request. A model that re-opens one it
    /// has seen gets it back for free: without this, a repeat fetch of the
    /// same kruidvat page ate a third of the budget and left none for the
    /// Action listing the user was asking about.
    seen: std::sync::Mutex<std::collections::HashMap<String, PageContent>>,
    hosts: std::sync::Mutex<Hosts>,
}

/// What this request has learnt of the hosts it asked, each keyed as
/// `host_key` writes it.
#[derive(Default)]
struct Hosts {
    /// Hosts a page was read from. One of these is never closed: a shop
    /// that has served a page is not refusing its reader, whatever one of
    /// its pages says.
    served: std::collections::HashSet<String>,
    /// Hosts that told this reader no — a block notice or a bot check —
    /// with nobody left to ask. The error says not to open more pages
    /// there; a model may open them all the same (one driving a browser
    /// did, on 2026-11-18, at a shop that had refused it), and each would
    /// cost a request, a Chrome and a fifth of the page budget to be told
    /// the same. So they are not opened.
    closed: std::collections::HashSet<String>,
}

/// A host as the request's memory keys it: without a leading `www.`, as the
/// trace script names hosts.
fn host_key(url: &Url) -> Option<String> {
    url.host_str().map(|host| host.strip_prefix("www.").unwrap_or(host).to_string())
}

/// How a site told its reader no, when it is the site and not the page.
#[derive(Clone, Copy, PartialEq)]
enum Wall {
    /// A bot check in place of the page, which no browser cleared.
    BotCheck,
    /// The site's own notice that it will not serve this reader.
    Notice,
}

impl FetchPageTool {
    pub fn new(web: crate::net_guard::OpenWeb, renderer: Option<super::browser::Renderer>) -> Self {
        Self {
            web,
            renderer,
            opens: std::sync::atomic::AtomicUsize::new(0),
            seen: std::sync::Mutex::new(std::collections::HashMap::new()),
            hosts: std::sync::Mutex::new(Hosts::default()),
        }
    }

    /// Keeps a page for the rest of the request and hands back a copy.
    /// `from` is where it was read.
    fn remember(&self, key: String, from: &Url, page: PageContent) -> PageContent {
        if let Some(host) = host_key(from) {
            self.hosts.lock().unwrap().served.insert(host);
        }
        self.seen.lock().unwrap().insert(key, page.clone());
        page
    }

    /// The error for a site that told its reader no. Handing a wall's text
    /// back as page content is what let the model mistake it for a page with
    /// nothing on it and open three more just like it, so it is told in
    /// words. `at` is where the request ended up, which after a redirect is
    /// not where it was sent, and `named` is what to call that host to the
    /// model. `settled` is whether nobody was left to ask; it closes the
    /// host for the rest of the request, unless the host has served a page
    /// in it — and then the model is told of the page, not of the site.
    fn unreadable(&self, at: &Url, named: &Url, wall: Wall, settled: bool) -> FetchError {
        let mut reads_fine = false;
        if let Some(host) = host_key(at) {
            let mut hosts = self.hosts.lock().unwrap();
            reads_fine = hosts.served.contains(&host);
            if settled && !reads_fine {
                hosts.closed.insert(host);
            }
        }
        let what = match wall {
            Wall::BotCheck => "served a bot check instead of the page, and it could not be cleared",
            Wall::Notice => "served a block notice instead of the page",
        };
        let then = if reads_fine {
            "this page is unreadable, though others of the site were read; use what the search \
             results already told you of it"
        } else {
            "this site is unreadable, so do not open more pages on it; use what the search results \
             already told you"
        };
        FetchError::Invalid(format!("{} {what} — {then}", named.host_str().unwrap_or("the site")))
    }
}

/// Whether what was read is the site's notice that it will not serve this
/// reader, and not the page. A page whose data prices its product was
/// served, whatever else it says.
pub(crate) fn is_refusal(page: &PageContent) -> bool {
    let priced = page.product.as_ref().is_some_and(|p| p.price.is_some());
    !priced && super::refusal::says_no(&page.text)
}

/// Reddit's own site is a JavaScript shell behind a bot wall, but the old
/// interface serves the same thread fully rendered — post, comments and all
/// — to a plain GET. Measured on one thread: www gives 8 KB of interstitial,
/// old gives 69 KB with every comment in it.
///
/// Only the HTML hosts are switched. `i.redd.it` and friends serve media and
/// have no old-interface equivalent.
fn readable_url(url: &Url) -> Option<Url> {
    if !matches!(url.host_str(), Some("reddit.com" | "www.reddit.com")) {
        return None;
    }
    let mut old = url.clone();
    old.set_host(Some("old.reddit.com")).ok()?;
    Some(old)
}

/// Whether a headless render is worth trying after a plain fetch. `page` is
/// what the body was read as, where there was a body to read.
///
/// 404/410 mean the page is gone — rendering cannot bring it back, and
/// pretending otherwise would undo the dead-link checks. A bot wall (403,
/// 429, 503) or a challenge page served with HTTP 200 is exactly what a real
/// browser is for.
pub(crate) fn worth_rendering(status: Option<u16>, body: &str, page: Option<&PageContent>) -> bool {
    match status {
        Some(404 | 410) => false,
        Some(403 | 429 | 503) | None => true,
        Some(s) if !(200..300).contains(&s) => true,
        // A success that carries no readable content: a challenge, a shell
        // whose body arrives by script, or the site's notice that it will
        // not serve this reader, which a browser is not always shown.
        _ => super::browser::looks_unrendered(body) || page.is_some_and(is_refusal),
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
        // Some hosts have a second address that answers plain GETs properly.
        let fetched = readable_url(&url).unwrap_or_else(|| url.clone());
        // A host that has told this request no is not asked again: see
        // `Hosts::closed`. Before the budget, since nothing is opened.
        if host_key(&fetched).is_some_and(|host| self.hosts.lock().unwrap().closed.contains(&host)) {
            tracing::info!(url = %url, "the host refused a page earlier in this request; not opened");
            return Err(FetchError::Invalid(format!(
                "{} already refused a page in this request, so this one was not opened again — \
                 use what the search results already told you",
                url.host_str().unwrap_or("the site")
            )));
        }
        // Before the budget and before the renderer: an address inside the
        // pod or the cluster is not a page that failed, it is a page that
        // does not exist for us, and the model should hear that in words
        // rather than as a connector error it might retry.
        if self.web.guard.check(&url).await.is_err() {
            tracing::warn!(url = %url, "refused a non-public address");
            return Err(FetchError::Invalid(crate::net_guard::NOT_PUBLIC.to_string()));
        }
        // Counted only once the url is worth opening, so a malformed one
        // costs the model a turn but not part of its page budget.
        if self.opens.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= MAX_OPENS {
            return Err(FetchError::Invalid(format!(
                "page budget spent ({MAX_OPENS} opens per request) — answer with what you \
                 already have instead of opening more pages"
            )));
        }
        if fetched != url {
            tracing::info!(url = %url, via = %fetched, "using the readable host");
        }
        // A redirect hop or a rebinding name is refused inside the client;
        // this only translates that refusal into the same sentence.
        let resp = browser_get(&self.web.http, fetched.clone()).send().await.map_err(|e| {
            if crate::net_guard::refused(&e) {
                tracing::warn!(url = %url, "refused a redirect to a non-public address");
                FetchError::Invalid(crate::net_guard::NOT_PUBLIC.to_string())
            } else {
                FetchError::Http(e)
            }
        })?;
        let status = resp.status();
        // Where the request ended up: after a redirect, the host that
        // answered is not the one that was asked, and what is learnt is
        // learnt of the one that answered.
        let landed = resp.url().clone();
        let body = if status.is_success() { resp.text().await? } else { String::new() };
        // Read once: what the plain request brought decides whether a
        // browser is sent after it, and is what is handed back if not.
        let plain = status.is_success().then(|| extract_page(&body, &fetched));
        // What either reader was told in place of the page.
        let mut wall = match &plain {
            Some(page) if is_refusal(page) => Some(Wall::Notice),
            Some(_) if super::browser::is_challenge_page(&body) => Some(Wall::BotCheck),
            _ => None,
        };

        // Plain HTTP could not read this page; a real browser sometimes can.
        // `answered` is a browser that came back with anything at all: one
        // that timed out or found no free slot says nothing about this page
        // or the next.
        let mut answered = false;
        if let Some(renderer) = self
            .renderer
            .as_ref()
            .filter(|_| worth_rendering(Some(status.as_u16()), &body, plain.as_ref()))
        {
            tracing::info!(url = %url, status = status.as_u16(), "retrying with headless browser");
            match renderer.render(fetched.as_str()).await {
                Ok(html) => {
                    answered = true;
                    let page = extract_page(&html, &fetched);
                    if is_refusal(&page) {
                        // bol.com shows the browser its notice as an
                        // ordinary page of the shop, 9 KB of it.
                        tracing::info!(url = %url, "the browser was shown a block notice");
                        wall = Some(Wall::Notice);
                    } else if super::browser::looks_unrendered(&html) {
                        tracing::info!(url = %url, "render produced no usable page");
                        if super::browser::is_challenge_page(&html) {
                            wall = wall.or(Some(Wall::BotCheck));
                        }
                    } else {
                        return Ok(self.remember(key, &landed, page));
                    }
                }
                Err(e) => tracing::warn!(url = %url, error = %e, "render failed"),
            }
        }
        // Told no, by the plain answer or by what the browser was shown —
        // whichever status came with it. With no browser there is nobody
        // left to ask; with one, only if it answered. The host is called
        // what the model called it, unless a redirect took the request to
        // another: reddit's old interface is still reddit to whoever asked.
        if let Some(wall) = wall {
            let named = if host_key(&landed) == host_key(&fetched) { &url } else { &landed };
            return Err(self.unreadable(&landed, named, wall, self.renderer.is_none() || answered));
        }
        // A status alone is about this page: a 403 may be one directory and
        // a 503 one bad minute, so it is reported as it was and closes
        // nothing.
        let Some(page) = plain else {
            return Err(FetchError::Invalid(format!("page returned HTTP {status}")));
        };
        Ok(self.remember(key, &landed, page))
    }
}

#[cfg(test)]
pub fn extract_page_for_test(html: &str, url: &str) -> Option<PageProduct> {
    extract_page(html, &Url::parse(url).unwrap()).product
}

pub(crate) fn extract_page(html: &str, base: &Url) -> PageContent {
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
    use crate::net_guard::OpenWeb;
    use std::time::Duration;
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

        let tool = FetchPageTool::new(OpenWeb::exempting(*server.address()), None);
        let page = tool
            .call(FetchArgs { url: format!("{}/p/1", server.uri()) })
            .await
            .unwrap();
        assert!(page.text.contains("Widget"));
        assert_eq!(page.links.len(), 1);
        assert!(page.links[0].url.ends_with("/p/2"));
    }

    #[tokio::test]
    async fn a_bot_wall_that_answers_200_is_an_error_not_a_page() {
        // Reddit's wall, verbatim in shape: HTTP 200, real-looking shell, no
        // content. Returned as text it reads like a page that happened to be
        // empty, and the model opens more of the same — which is how one run
        // walked into a five-minute stall.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/thread"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "<html><head><title>Reddit - Please wait for verification</title></head>\
                 <body></body></html>",
            ))
            .mount(&server)
            .await;

        // No renderer configured, so the wall cannot be cleared.
        let tool = FetchPageTool::new(OpenWeb::exempting(*server.address()), None);
        let err = tool
            .call(FetchArgs { url: format!("{}/thread", server.uri()) })
            .await
            .expect_err("a bot check is not a page");
        let msg = err.to_string();
        assert!(msg.contains("bot check"), "got: {msg}");
        assert!(
            msg.contains("do not open more pages"),
            "the model needs to be told to stop, not just that this one failed: {msg}"
        );
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
    fn reddit_threads_are_read_through_the_old_interface() {
        let old = |u: &str| readable_url(&Url::parse(u).unwrap()).map(String::from);

        // Path, query and fragment survive; only the host moves.
        assert_eq!(
            old("https://www.reddit.com/r/teenageengineering/comments/1hkpmq6/whos_got_the_cm15/"),
            Some(
                "https://old.reddit.com/r/teenageengineering/comments/1hkpmq6/whos_got_the_cm15/"
                    .to_string()
            )
        );
        assert_eq!(
            old("https://reddit.com/r/x/comments/1/y/?sort=top"),
            Some("https://old.reddit.com/r/x/comments/1/y/?sort=top".to_string())
        );

        // Media hosts have no old-interface equivalent, and everything else
        // is none of our business.
        assert_eq!(old("https://i.redd.it/abc.jpg"), None);
        assert_eq!(old("https://preview.redd.it/abc.jpg"), None);
        assert_eq!(old("https://old.reddit.com/r/x/"), None, "already readable");
        assert_eq!(old("https://www.bol.com/nl/nl/p/x/123/"), None);
    }

    #[test]
    fn rendering_is_reserved_for_pages_plain_http_cannot_read() {
        let real_page = "<html><body>".to_string() + &"content ".repeat(500) + "</body></html>";

        // Bot walls and challenges are what a browser is for.
        assert!(worth_rendering(Some(403), "", None));
        assert!(worth_rendering(Some(429), "", None));
        assert!(worth_rendering(Some(503), "", None));
        assert!(worth_rendering(Some(200), "<html><title>Just a moment...</title></html>", None));
        assert!(worth_rendering(Some(200), "<html><body>tiny shell</body></html>", None));

        // A page that is simply gone stays gone: rendering it would undo the
        // dead-link checks.
        assert!(!worth_rendering(Some(404), "", None));
        assert!(!worth_rendering(Some(410), "", None));
        // A page we could read needs no browser.
        let read = extract_page(&real_page, &base());
        assert!(!worth_rendering(Some(200), &real_page, Some(&read)));

        // The shop's own notice that it will not serve this reader is a
        // page by its size and its title, and a browser is sometimes served
        // where the plain request was not: amazon.nl, 2026-11-18.
        let notice = block_notice();
        let read = extract_page(&notice, &base());
        assert!(worth_rendering(Some(200), &notice, Some(&read)));
        // A page that is gone is gone whatever it says.
        assert!(!worth_rendering(Some(404), &notice, Some(&read)));
    }

    /// Shaped like the block notice bol.com showed a browser on 2026-11-18:
    /// an ordinary document of the shop, 9 KB with its styles, a title that
    /// says nothing, a heading and a paragraph.
    fn block_notice() -> String {
        format!(
            "<!DOCTYPE html><html><head><title>bol</title><style>{}</style></head><body>\
             <div id=\"message\"><h1>Access blocked</h1><p>Your access to bol has been \
             temporarily blocked due to possible abuse.</p></div></body></html>",
            ".unicorn{display:block}".repeat(400)
        )
    }

    /// The robot check amazon.nl answered a plain request with: 2 KB, so a
    /// shell by its size, and its words inside a `noscript`.
    const ROBOT_CHECK: &str = "<!DOCTYPE html><html lang=\"en\"><head><title></title></head><body>\
        <div id=\"challenge-container\"></div><noscript><h1>JavaScript is disabled</h1>In order to \
        continue, we need to verify that you're not a robot. This requires JavaScript. Enable \
        JavaScript and then reload the page.</noscript></body></html>";

    /// A page that is all script: nothing to read until a browser runs it.
    const SHELL: &str = "<html><head><title>Shop</title></head><body><div id=\"root\"></div></body></html>";

    const CHALLENGE: &str = "<html><head><title>Just a moment...</title></head><body></body></html>";

    /// A product page as a browser hands it over: long, and priced in its data.
    fn product_page() -> String {
        format!(
            r#"<html><head><title>Widget</title><script type="application/ld+json">
               {{"@type":"Product","name":"Widget","offers":{{"@type":"Offer","price":"9.99",
               "priceCurrency":"EUR","availability":"https://schema.org/InStock"}}}}</script></head>
               <body><h1>Widget</h1><p>9,99</p><p>{}</p></body></html>"#,
            "A widget for every purpose. ".repeat(100)
        )
    }

    async fn serving(status: u16, body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&server)
            .await;
        server
    }

    fn answering(html: &str) -> Option<crate::tools::browser::Renderer> {
        Some(crate::tools::browser::Renderer::answering(html))
    }

    /// A browser that never answers: it will not start.
    fn silent() -> Option<crate::tools::browser::Renderer> {
        Some(crate::tools::browser::Renderer::new(std::path::PathBuf::from("/definitely/not/chrome")))
    }

    fn tool_with(server: &MockServer, renderer: Option<crate::tools::browser::Renderer>) -> FetchPageTool {
        FetchPageTool::new(OpenWeb::exempting(*server.address()), renderer)
    }

    async fn requests(server: &MockServer) -> usize {
        server.received_requests().await.expect("the mock records what it is asked").len()
    }

    /// Asks for `/p/1` and then `/p/2`, and says what the first was answered
    /// with and whether the second was opened at all.
    async fn two_pages(tool: &FetchPageTool, server: &MockServer) -> (String, bool) {
        let first = tool
            .call(FetchArgs { url: format!("{}/p/1", server.uri()) })
            .await
            .expect_err("the first page is refused")
            .to_string();
        let before = requests(server).await;
        let second = tool.call(FetchArgs { url: format!("{}/p/2", server.uri()) }).await;
        let opened = requests(server).await > before;
        if !opened {
            let msg = second.expect_err("a page that was not opened is not a page").to_string();
            assert!(msg.contains("already refused a page in this request"), "got: {msg}");
            assert!(msg.contains("was not opened again"), "got: {msg}");
        }
        (first, opened)
    }

    const NOTICE_SAID: &str = "served a block notice instead of the page";
    const BOT_CHECK_SAID: &str = "served a bot check instead of the page";

    #[test]
    fn the_fixtures_are_what_the_tests_take_them_for() {
        // Or the tests below would pass for the wrong reason.
        let notice = block_notice();
        assert!(notice.len() > 2048);
        assert!(!crate::tools::browser::looks_unrendered(&notice), "a page by its size and its title");
        assert!(!crate::tools::browser::is_challenge_page(&notice));
        assert!(is_refusal(&extract_page(&notice, &base())));

        assert!(crate::tools::browser::looks_unrendered(ROBOT_CHECK), "a shell by its size");
        assert!(!crate::tools::browser::is_challenge_page(ROBOT_CHECK));
        assert!(is_refusal(&extract_page(ROBOT_CHECK, &base())));

        assert!(crate::tools::browser::looks_unrendered(SHELL));
        assert!(!crate::tools::browser::is_challenge_page(SHELL));
        assert!(!is_refusal(&extract_page(SHELL, &base())));

        assert!(crate::tools::browser::is_challenge_page(CHALLENGE));
        assert!(!is_refusal(&extract_page(CHALLENGE, &base())));
        assert!(!is_refusal(&extract_page(&product_page(), &base())));
    }

    #[tokio::test]
    async fn a_block_notice_that_answers_200_is_an_error_not_a_page() {
        // No browser installed, so nobody else can ask.
        let server = serving(200, &block_notice()).await;
        let tool = tool_with(&server, None);
        let err = tool
            .call(FetchArgs { url: format!("{}/p/1", server.uri()) })
            .await
            .expect_err("a notice that the reader is blocked is not the page");
        let msg = err.to_string();
        assert!(msg.contains(NOTICE_SAID), "got: {msg}");
        assert!(msg.contains("do not open more pages"), "the model is told to stop: {msg}");
    }

    #[tokio::test]
    async fn a_notice_to_the_plain_request_goes_to_the_browser_and_what_it_is_served_is_the_page() {
        // amazon.nl, 2026-11-18: the notice to the plain request, the
        // product page to a browser.
        let server = serving(200, &block_notice()).await;
        let tool = tool_with(&server, answering(&product_page()));
        let page = tool.call(FetchArgs { url: format!("{}/dp/1", server.uri()) }).await.unwrap();
        assert_eq!(page.product.and_then(|p| p.price).as_deref(), Some("9.99 EUR"));
        assert!(page.text.contains("A widget for every purpose"));
    }

    #[tokio::test]
    async fn a_browser_shown_the_notice_is_not_handed_back_as_the_page_whatever_the_plain_answer_was() {
        // bol.com on 2026-11-18: 403 to the plain request, and to the
        // browser sent after it the block notice, which was returned as the
        // product page. A shell and a failure of the server's own are the
        // same case: what the browser was shown is a notice.
        for (status, plain) in [(403, ""), (200, SHELL), (500, ""), (200, CHALLENGE)] {
            let server = serving(status, plain).await;
            let tool = tool_with(&server, answering(&block_notice()));
            let (said, opened) = two_pages(&tool, &server).await;
            assert!(said.contains(NOTICE_SAID), "{status}: {said}");
            assert!(said.contains("do not open more pages"), "{status}: {said}");
            assert!(!opened, "{status}: the browser answered, so nobody is left to ask");
        }
    }

    #[tokio::test]
    async fn a_notice_the_size_of_a_shell_is_a_notice_all_the_same() {
        // amazon.nl's robot check is 2 KB. From the plain request it goes
        // to the browser as any shell does; shown to the browser as well, it
        // is not the page for being small.
        let server = serving(202, ROBOT_CHECK).await;
        let (said, opened) = two_pages(&tool_with(&server, answering(ROBOT_CHECK)), &server).await;
        assert!(said.contains(NOTICE_SAID), "got: {said}");
        assert!(!opened);
        // And where only the browser was shown it.
        let server = serving(403, "").await;
        let (said, opened) = two_pages(&tool_with(&server, answering(ROBOT_CHECK)), &server).await;
        assert!(said.contains(NOTICE_SAID), "got: {said}");
        assert!(!opened);
        // With no browser it is what the plain request was told.
        let server = serving(202, ROBOT_CHECK).await;
        let (said, opened) = two_pages(&tool_with(&server, None), &server).await;
        assert!(said.contains(NOTICE_SAID), "got: {said}");
        assert!(!opened);
    }

    #[tokio::test]
    async fn a_bot_check_nobody_cleared_is_said_and_closes_the_host() {
        // To the plain request with no browser; to the plain request with a
        // browser that came back with a shell; and to the browser, after a
        // status that said nothing.
        for (status, plain, browser) in [
            (200, CHALLENGE, None),
            (200, CHALLENGE, answering(SHELL)),
            (200, CHALLENGE, answering(CHALLENGE)),
            (403, "", answering(CHALLENGE)),
        ] {
            let sent = browser.is_some();
            let server = serving(status, plain).await;
            let (said, opened) = two_pages(&tool_with(&server, browser), &server).await;
            assert!(said.contains(BOT_CHECK_SAID), "{status}, browser {sent}: {said}");
            assert!(!opened, "{status}, browser {sent}");
        }
    }

    #[tokio::test]
    async fn a_refusal_the_browser_never_answered_leaves_the_host_open() {
        // The browser was busy, timed out or would not start: nothing was
        // learnt about what it would have been shown, here or on the next
        // page.
        for (plain, said_as) in [(block_notice(), NOTICE_SAID), (CHALLENGE.to_string(), BOT_CHECK_SAID)] {
            let server = serving(200, &plain).await;
            let (said, opened) = two_pages(&tool_with(&server, silent()), &server).await;
            assert!(said.contains(said_as), "got: {said}");
            assert!(opened, "the second page was asked for: {said_as}");
        }
    }

    #[tokio::test]
    async fn a_status_alone_is_reported_as_it_was_and_closes_nothing() {
        // A 403 may be one directory and a 503 one bad minute. Whether no
        // browser was sent, the browser never answered, or it came back
        // with the server's own few bytes of error page: nobody was told no
        // as a reader, so the next page is asked for.
        let stock = "<html><head><title>403 Forbidden</title></head><body><h1>403 Forbidden</h1><hr>nginx</body></html>";
        for status in [403, 429, 503, 500, 401] {
            for browser in [None, silent(), answering(stock), answering(SHELL)] {
                let sent = browser.is_some();
                let server = serving(status, "").await;
                let (said, opened) = two_pages(&tool_with(&server, browser), &server).await;
                assert!(said.starts_with(&format!("page returned HTTP {status}")), "browser {sent}: {said}");
                assert!(opened, "{status}, browser {sent}");
            }
        }
    }

    #[tokio::test]
    async fn a_page_that_is_gone_is_never_taken_to_the_browser() {
        let server = serving(404, "").await;
        // A browser that would answer with a page, were it asked.
        let (said, opened) = two_pages(&tool_with(&server, answering(&product_page())), &server).await;
        assert!(said.starts_with("page returned HTTP 404"), "got: {said}");
        assert!(opened);
    }

    #[tokio::test]
    async fn a_host_that_refused_is_not_opened_again_in_the_request() {
        // Each page of a shop that has said no would cost a request, a
        // Chrome and a fifth of the page budget, to be told the same.
        let server = serving(403, "").await;
        let tool = tool_with(&server, answering(&block_notice()));
        let first = format!("{}/p/1", server.uri());
        assert!(tool.call(FetchArgs { url: first.clone() }).await.is_err());
        assert_eq!(requests(&server).await, 1);

        for url in [format!("{}/p/2", server.uri()), format!("{}/s?q=widget", server.uri()), first] {
            let err = tool.call(FetchArgs { url: url.clone() }).await.expect_err("the host said no");
            let msg = err.to_string();
            assert!(msg.contains("already refused a page in this request"), "{url}: {msg}");
            assert!(msg.contains("was not opened again"), "{url}: {msg}");
        }
        assert_eq!(requests(&server).await, 1, "nothing more was sent to it");
        assert_eq!(
            tool.opens.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "and a page that was not opened costs none of the budget"
        );

        // The next request asks again: a refusal is remembered for a
        // request, not for good.
        let next = tool_with(&server, answering(&block_notice()));
        assert!(next.call(FetchArgs { url: format!("{}/p/2", server.uri()) }).await.is_err());
        assert_eq!(requests(&server).await, 2);
    }

    #[tokio::test]
    async fn a_host_that_has_served_a_page_is_not_closed_by_one_that_says_no() {
        // A shop's product page, then its account page: "access blocked"
        // there is about that page. The shop is reading fine.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/account"))
            .respond_with(ResponseTemplate::new(200).set_body_string(block_notice()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(product_page()))
            .mount(&server)
            .await;
        let tool = tool_with(&server, None);
        let page = |n: u8| FetchArgs { url: format!("{}/p/{n}", server.uri()) };
        assert!(tool.call(page(1)).await.is_ok());
        let err = tool.call(FetchArgs { url: format!("{}/account", server.uri()) }).await.unwrap_err().to_string();
        assert!(err.contains(NOTICE_SAID), "that page is still not a page: {err}");
        // And the model is not told to keep away from a shop that reads fine.
        assert!(err.contains("this page is unreadable"), "got: {err}");
        assert!(!err.contains("this site is unreadable") && !err.contains("do not open more pages"), "got: {err}");
        assert!(tool.call(page(2)).await.is_ok(), "and the shop is still open");
        assert!(tool.call(page(1)).await.is_ok());
        assert_eq!(requests(&server).await, 3);
    }

    #[tokio::test]
    async fn a_refusal_is_remembered_of_the_host_that_answered() {
        // After a redirect that is not the host that was asked: a
        // comparison site's link out to a shop that says no closes the
        // shop, and says so of the shop. (Not driven through a redirect
        // here: the test guard lets one socket through, and a second host
        // needs a second.)
        let tool = FetchPageTool::new(OpenWeb::new(Duration::from_secs(5)).unwrap(), None);
        let landed = Url::parse("https://www.shop.example/blocked").unwrap();
        let said = tool.unreadable(&landed, &landed, Wall::Notice, true).to_string();
        assert!(said.starts_with("www.shop.example served a block notice"), "got: {said}");
        // Its pages are refused before anything is sent, with the www or without.
        for url in ["https://www.shop.example/p/2", "https://shop.example/p/3"] {
            let err = tool.call(FetchArgs { url: url.into() }).await.unwrap_err().to_string();
            assert!(err.contains("was not opened again"), "{url}: {err}");
        }

        // Reddit is asked at its old interface whichever address the model
        // wrote, so a refusal there closes all of them, and is said of the
        // address the model wrote.
        let old = Url::parse("https://old.reddit.com/r/x/comments/1").unwrap();
        let asked = Url::parse("https://www.reddit.com/r/x/comments/1").unwrap();
        let said = tool.unreadable(&old, &asked, Wall::BotCheck, true).to_string();
        assert!(said.starts_with("www.reddit.com served a bot check"), "got: {said}");
        for url in ["https://www.reddit.com/r/y", "https://reddit.com/r/z", "https://old.reddit.com/r/x"] {
            let err = tool.call(FetchArgs { url: url.into() }).await.unwrap_err().to_string();
            assert!(err.contains("was not opened again"), "{url}: {err}");
            assert!(err.starts_with(Url::parse(url).unwrap().host_str().unwrap()), "{url}: {err}");
        }
        assert_eq!(tool.opens.load(std::sync::atomic::Ordering::Relaxed), 0, "none of these cost a page");

        // A refusal with somebody left to ask is said and closes nothing.
        let other = Url::parse("https://other.example/p").unwrap();
        assert!(tool.unreadable(&other, &other, Wall::BotCheck, false).to_string().contains(BOT_CHECK_SAID));
        let hosts = tool.hosts.lock().unwrap();
        assert!(hosts.closed.contains("shop.example") && hosts.closed.contains("old.reddit.com"));
        assert!(!hosts.closed.contains("other.example"));
    }

    #[test]
    fn a_host_is_remembered_without_its_www_and_reddit_by_the_address_that_is_asked() {
        let key = |url: &str| host_key(&Url::parse(url).unwrap());
        assert_eq!(key("https://www.shop.example/p/1"), key("https://shop.example/s?q=x"));
        assert_eq!(key("https://www.shop.example/p/1").as_deref(), Some("shop.example"));
        assert_ne!(key("https://m.shop.example/p/1"), key("https://shop.example/p/1"));
        // Reddit is asked at its old interface, whichever address the model
        // wrote: so that is the host a refusal is remembered of.
        let asked = |url: &str| {
            let url = Url::parse(url).unwrap();
            host_key(&readable_url(&url).unwrap_or(url))
        };
        assert_eq!(asked("https://www.reddit.com/r/x/comments/1"), asked("https://reddit.com/r/y"));
        assert_eq!(asked("https://www.reddit.com/r/x").as_deref(), Some("old.reddit.com"));
    }

    #[tokio::test]
    async fn a_priced_page_is_the_page_whatever_else_it_says() {
        // Short, and with the words of a refusal in it; but a page whose
        // data prices its product was served.
        let page = r#"<html><head><title>Widget</title><script type="application/ld+json">
            {"@type":"Product","name":"Widget","offers":{"@type":"Offer","price":"13.80","priceCurrency":"EUR"}}
            </script></head><body><h1>Widget</h1><p>13,80</p><p>Returns: the old portal is temporarily blocked.</p>"#
            .to_string()
            + &format!("<!-- {} --></body></html>", "padding ".repeat(400));
        let server = serving(200, &page).await;
        let tool = tool_with(&server, None);
        let read = tool.call(FetchArgs { url: format!("{}/p/1", server.uri()) }).await.unwrap();
        assert!(crate::tools::refusal::says_no(&read.text), "its text alone would be a refusal: {}", read.text);
        assert_eq!(read.product.and_then(|p| p.price).as_deref(), Some("13.80 EUR"));
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

        let tool = FetchPageTool::new(OpenWeb::exempting(*server.address()), None);
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

        let tool = FetchPageTool::new(OpenWeb::exempting(*server.address()), None);
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
        let next = FetchPageTool::new(OpenWeb::exempting(*server.address()), None);
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

        let tool = FetchPageTool::new(OpenWeb::exempting(*server.address()), None);
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

    #[tokio::test]
    async fn an_address_inside_the_pod_is_refused_before_anything_is_sent() {
        // What a prompt injection on a page Scout reads would ask for: the
        // pod's own metrics listener, echoed back into the chat.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("secret_total 1"))
            .expect(0)
            .mount(&server)
            .await;

        let tool = FetchPageTool::new(OpenWeb::new(Duration::from_secs(5)).unwrap(), None);
        for url in [
            format!("{}/metrics", server.uri()),
            "http://169.254.169.254/latest/meta-data/".to_string(),
            "http://10.43.0.1/".to_string(),
            "http://localhost:9090/metrics".to_string(),
        ] {
            let err = tool.call(FetchArgs { url: url.clone() }).await.unwrap_err();
            assert!(matches!(err, FetchError::Invalid(_)), "{url}: {err:?}");
            assert!(err.to_string().contains("not on the public internet"), "{url}: {err}");
        }
        // Refusals are not opens: the budget is untouched.
        assert_eq!(tool.opens.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn a_public_page_that_redirects_into_the_pod_is_refused() {
        let public = MockServer::start().await;
        let private = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("secret_total 1"))
            .expect(0)
            .mount(&private)
            .await;
        Mock::given(method("GET"))
            .and(path("/innocent"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/metrics", private.uri())),
            )
            .mount(&public)
            .await;

        let tool = FetchPageTool::new(OpenWeb::exempting(*public.address()), None);
        let err = tool
            .call(FetchArgs { url: format!("{}/innocent", public.uri()) })
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::Invalid(_)), "got: {err:?}");
        assert!(err.to_string().contains("not on the public internet"), "got: {err}");
    }
}
