//! What `fetch_page` makes of a page, for measuring from outside a run.
//!
//! `tools` is private and stays so: nothing here hands a channel a tool.
//! This is for `examples/page_as_read.rs`, which reads a page the way
//! `fetch_page` would and prints what the model would have been handed, so
//! that a measurement (`scripts/shop-reach.mjs`) judges a page by the
//! extractor itself and not by a copy of it that has drifted.
//!
//! And for `examples/delivery_as_read.rs`, which reads a shop's delivery
//! terms the way the delivery desk would, for trying its prompt on a real
//! page and a real model.

use crate::tools::{browser, delivery, fetch};

/// The headers `fetch_page` sends with its plain request.
pub fn request_headers() -> [(&'static str, &'static str); 3] {
    [
        ("User-Agent", fetch::BROWSER_USER_AGENT),
        ("Accept", fetch::BROWSER_ACCEPT),
        ("Accept-Language", fetch::BROWSER_ACCEPT_LANGUAGE),
    ]
}

/// What `fetch_page` would make of `html`, served at `url` with `status`
/// (`None` for HTML that is a render's own output and so has none):
///
/// - `would_render`: whether it would have gone on to the headless render,
///   or null where there is no status to decide by;
/// - `unrendered`: whether, were this the render's own output, it would
///   have been thrown away as a shell or a bot check;
/// - `challenge`: whether it would have refused the body as a bot check;
/// - `refusal`: whether it is the site's own notice that it will not serve
///   this reader. From the plain request that sends a browser after it;
///   as the render's own output it is the end of the read;
/// - `page`: what it hands the model — text, links, stock, product — when
///   none of the above stands in the way.
pub fn page_as_read(html: &str, url: &str, status: Option<u16>) -> anyhow::Result<serde_json::Value> {
    let base = url::Url::parse(url).map_err(|e| anyhow::anyhow!("invalid url: {e}"))?;
    let page = fetch::extract_page(html, &base);
    Ok(serde_json::json!({
        "would_render": status.map(|s| fetch::worth_rendering(Some(s), html, Some(&page))),
        "unrendered": browser::looks_unrendered(html),
        "challenge": browser::is_challenge_page(html),
        "refusal": fetch::is_refusal(&page),
        "page": page,
    }))
}

/// What the delivery desk would make of a shop today, read with the side
/// model `side` names, and stored nowhere:
///
/// - `links`: the shop's pages about delivery among the links of `page`,
///   best first, and `page`: the one that was asked for;
/// - `text`: what the model was handed of it, and `text_chars`;
/// - `answer`: what the model said, as it said it;
/// - `kept`: what Rust made of that — a status, and the bands and currency
///   when it is terms;
/// - `refused`: the check that refused the answer, or what failed before
///   there was one;
/// - `side_call`: how the question ended, how long it took and what it
///   cost in tokens, as `/metrics` would count it.
///
/// `page` is any page of the shop that links its delivery page, as an
/// offer's address would be. `country` is a code or a name. The request
/// does not go through `net_guard`: the address comes from whoever runs
/// this, not from a model. Neither the key nor any header is in the result.
pub async fn delivery_as_read(page: &str, country: &str, side: &crate::config::SideLlm) -> anyhow::Result<serde_json::Value> {
    let offer = url::Url::parse(page).map_err(|e| anyhow::anyhow!("invalid url: {e}"))?;
    let host = fetch::host_key(&offer).ok_or_else(|| anyhow::anyhow!("the address names no host"))?;
    let country = delivery::country(country)
        .ok_or_else(|| anyhow::anyhow!("no country is known here as {country:?}; give a code or a name, such as NL or Netherlands"))?;
    // The timeout `Core` gives its clients.
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build()?;
    let model = crate::agent::SideModel { llm: crate::agent::llm_client(&side.api_key, &side.base_url)?, name: side.model.clone() };
    // A set of its own: this question is no run's, and its counts are
    // read back below.
    let counts = crate::metrics::Metrics::default();
    let started = std::time::Instant::now();
    let reading = delivery::read_shop(delivery::Reach::Unguarded(&http), &model, &counts, None, &host, &offer, country).await;
    let kept = match &reading.terms {
        delivery::Terms::Costs { bands, currency } => {
            serde_json::json!({"status": "terms", "bands": bands.pairs(), "currency": currency})
        }
        other => serde_json::json!({"status": other.status()}),
    };
    Ok(serde_json::json!({
        "shop": host,
        "country": country.code,
        "model": side.model,
        "links": reading.links,
        "page": reading.page,
        "text_chars": reading.text.as_ref().map(|text| text.chars().count()),
        "text": reading.text,
        "answer": reading.answer,
        "kept": kept,
        "refused": reading.detail,
        "side_call": side_call_counted(&counts.render()),
        "ms": started.elapsed().as_millis() as u64,
    }))
}

/// The one delivery side call a metrics page counted: how it ended, its
/// milliseconds and its tokens. Nothing when no model was asked. Read off
/// the page because a side call is counted and does not hand its usage
/// back; the set is one made for this question alone.
fn side_call_counted(page: &str) -> serde_json::Value {
    let ours = |series: &str| {
        let start = format!("{series}{{call=\"delivery\",");
        page.lines().filter(move |line| line.starts_with(&start))
    };
    let count = |line: &str| line.rsplit(' ').next().and_then(|n| n.parse::<u64>().ok());
    let outcome = ours("scout_side_calls_total")
        .find_map(|line| line.split("outcome=\"").nth(1)?.split('"').next().map(str::to_string));
    let Some(outcome) = outcome else { return serde_json::Value::Null };
    let tokens: serde_json::Map<String, serde_json::Value> = ["input", "cached", "output", "reasoning"]
        .iter()
        .map(|kind| {
            let line = ours("scout_side_call_tokens_total").find(|line| line.contains(&format!("kind=\"{kind}\"")));
            (kind.to_string(), line.and_then(count).into())
        })
        .collect();
    serde_json::json!({
        "outcome": outcome,
        "ms": ours("scout_side_call_ms_total").next().and_then(count),
        "tokens": tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRODUCT: &str = r#"<html><head><title>Widget</title>
        <script type="application/ld+json">
          {"@type":"Product","name":"Widget","offers":{"@type":"Offer","price":"13.80","priceCurrency":"EUR",
           "availability":"https://schema.org/InStock"}}
        </script></head><body><h1>Widget</h1><p>Nu voor 13,80</p></body></html>"#;

    fn padded(html: &str) -> String {
        format!("{html}<!-- {} -->", "padding ".repeat(400))
    }

    #[test]
    fn a_product_page_is_read_as_fetch_page_reads_it() {
        let read = page_as_read(&padded(PRODUCT), "https://shop.example/widget", Some(200)).unwrap();
        assert_eq!(read["page"]["product"]["price"], "13.80 EUR");
        assert_eq!(read["page"]["availability"], "in stock");
        assert!(read["page"]["text"].as_str().unwrap().contains("Nu voor 13,80"));
        assert_eq!(read["would_render"], false);
        assert_eq!(read["unrendered"], false);
        assert_eq!(read["challenge"], false);
        // A render's own output has no status, so there is nothing to say about going to the render.
        let rendered = page_as_read(&padded(PRODUCT), "https://shop.example/widget", None).unwrap();
        assert_eq!((rendered["would_render"].clone(), rendered["unrendered"].clone()), (serde_json::Value::Null, false.into()));
    }

    #[test]
    fn a_shell_a_wall_and_a_refusal_would_go_to_the_render_and_a_dead_page_would_not() {
        let shell = page_as_read("<html><body><div id=app></div></body></html>", "https://shop.example/p", Some(200)).unwrap();
        assert_eq!(shell["would_render"], true, "a body this small arrives by script");
        assert_eq!(shell["unrendered"], true);
        let wall = page_as_read(&padded("<html><head><title>Just a moment...</title></head><body></body></html>"), "https://shop.example/p", Some(200)).unwrap();
        assert_eq!((wall["would_render"].clone(), wall["challenge"].clone()), (true.into(), true.into()));
        assert_eq!(page_as_read("", "https://shop.example/p", Some(403)).unwrap()["would_render"], true);
        assert_eq!(page_as_read("", "https://shop.example/p", Some(404)).unwrap()["would_render"], false);
    }

    #[test]
    fn a_block_notice_is_told_apart_from_a_page_and_would_go_to_the_render() {
        // bol.com's notice in shape: nothing the older tests know it by.
        let notice = padded(
            "<html><head><title>bol</title></head><body><h1>IP address is blocked</h1><p>Your access \
             to bol has been temporarily blocked due to possible abuse from this IP address.</p></body></html>",
        );
        let read = page_as_read(&notice, "https://shop.example/p", Some(200)).unwrap();
        assert_eq!((read["unrendered"].clone(), read["challenge"].clone()), (false.into(), false.into()));
        assert_eq!(read["refusal"], true);
        assert_eq!(read["would_render"], true, "a browser is sometimes served where the plain request was not");
        // As a render's own output it is still a refusal: that is how the browser's answer is judged.
        assert_eq!(page_as_read(&notice, "https://shop.example/p", None).unwrap()["refusal"], true);
        assert_eq!(page_as_read(&padded(PRODUCT), "https://shop.example/widget", Some(200)).unwrap()["refusal"], false);
    }

    use crate::config::SideLlm;
    use crate::tools::delivery::fixtures::{self, product_page, World, DELIVERY, READ_RIGHT};

    fn side_at(w: &World) -> SideLlm {
        SideLlm { api_key: "sk-not-to-be-printed".into(), base_url: w.model.uri(), model: "side-model".into() }
    }

    #[tokio::test]
    async fn a_shops_delivery_terms_are_read_as_the_desk_reads_them_and_kept_nowhere() {
        let w = World::with_a_shop_that_reads_well().await;

        let read = delivery_as_read(w.url(fixtures::PRODUCT).as_str(), "Netherlands", &side_at(&w)).await.unwrap();

        let page = w.url(DELIVERY).to_string();
        assert_eq!((read["shop"].clone(), read["country"].clone(), read["model"].clone()), ("127.0.0.1".into(), "NL".into(), "side-model".into()));
        // What was found.
        assert_eq!(read["links"], serde_json::json!([page]));
        assert_eq!(read["page"], page.as_str());
        assert!(read["text"].as_str().unwrap().contains("De verzendkosten zijn € 5,49"), "{read}");
        assert_eq!(read["text_chars"], read["text"].as_str().unwrap().chars().count());
        // What the model answered, and what Rust kept of it.
        assert_eq!(read["answer"], READ_RIGHT);
        assert_eq!(read["kept"], serde_json::json!({"status": "terms", "bands": [[0.0, 5.49], [40.0, 0.0]], "currency": "EUR"}));
        assert_eq!(read["refused"], serde_json::Value::Null);
        // What the question cost, as the side call was counted.
        assert_eq!(read["side_call"]["outcome"], "ok");
        assert_eq!(read["side_call"]["tokens"], serde_json::json!({"input": 700, "cached": 0, "output": 40, "reasoning": 0}));
        assert!(read["side_call"]["ms"].is_u64() && read["ms"].is_u64(), "{read}");
        // Two plain requests and one question, as the desk makes; and it is
        // for measuring, so nothing is written down.
        assert_eq!(w.asked().await, (vec![fixtures::PRODUCT.to_string(), DELIVERY.to_string()], 1));
        assert!(w.rows().is_empty());
        assert!(!w.metrics.render().contains("call=\"delivery\""), "nor counted as a run's side call");
        // Neither the key nor a header is in what is printed.
        let line = read.to_string();
        assert!(!line.contains("sk-not-to-be-printed") && !line.contains("Bearer") && !line.to_lowercase().contains("authorization"), "{line}");
    }

    #[tokio::test]
    async fn what_rust_refused_and_what_failed_are_in_the_same_line() {
        // The model brings a cost the page does not write.
        let w = World::new().await;
        w.serve(fixtures::PRODUCT, 200, &product_page(Some(DELIVERY))).await;
        w.serve(DELIVERY, 200, &fixtures::delivery_page()).await;
        let made_up = r#"{"delivers": true, "by": "order_value", "bands": [{"from": 0, "cost": 4.95}], "currency": "EUR"}"#;
        w.model_says(made_up).await;
        let read = delivery_as_read(w.url(fixtures::PRODUCT).as_str(), "NL", &side_at(&w)).await.unwrap();
        assert_eq!(read["answer"], made_up, "what it said is there to be read");
        assert_eq!((read["kept"].clone(), read["refused"].clone()), (serde_json::json!({"status": "silent"}), "amount not on the page".into()));

        // The delivery page refuses: no model is asked, and the line says so.
        let w = World::new().await;
        w.serve(fixtures::PRODUCT, 200, &product_page(Some(DELIVERY))).await;
        w.serve(DELIVERY, 403, "").await;
        let read = delivery_as_read(w.url(fixtures::PRODUCT).as_str(), "NL", &side_at(&w)).await.unwrap();
        assert_eq!((read["kept"].clone(), read["refused"].clone()), (serde_json::json!({"status": "unread"}), "http 403".into()));
        assert_eq!((read["answer"].clone(), read["side_call"].clone(), read["text"].clone()), (serde_json::Value::Null, serde_json::Value::Null, serde_json::Value::Null));
        assert_eq!(read["page"], w.url(DELIVERY).as_str());

        // No delivery page among the shop's links.
        let w = World::new().await;
        w.serve(fixtures::PRODUCT, 200, &product_page(None)).await;
        let read = delivery_as_read(w.url(fixtures::PRODUCT).as_str(), "NL", &side_at(&w)).await.unwrap();
        assert_eq!((read["kept"].clone(), read["links"].clone(), read["page"].clone()), (serde_json::json!({"status": "no_page"}), serde_json::json!([]), serde_json::Value::Null));

        // What cannot be asked at all is an error, said plainly.
        let err = delivery_as_read(w.url(fixtures::PRODUCT).as_str(), "Atlantis", &side_at(&w)).await.unwrap_err();
        assert!(err.to_string().contains("Atlantis"), "{err}");
        assert!(delivery_as_read("not an address", "NL", &side_at(&w)).await.is_err());
    }

    #[test]
    fn an_address_that_is_not_one_is_an_error_not_a_page() {
        assert!(page_as_read(PRODUCT, "not an address", Some(200)).is_err());
    }

    #[test]
    fn the_headers_are_the_ones_the_plain_request_sends() {
        let headers = request_headers();
        assert_eq!(headers[0], ("User-Agent", fetch::BROWSER_USER_AGENT));
        assert_eq!(headers[1], ("Accept", fetch::BROWSER_ACCEPT));
        assert_eq!(headers[2], ("Accept-Language", fetch::BROWSER_ACCEPT_LANGUAGE));
    }
}
