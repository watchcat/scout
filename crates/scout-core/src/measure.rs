//! What `fetch_page` makes of a page, for measuring from outside a run.
//!
//! `tools` is private and stays so: nothing here hands a channel a tool.
//! This is for `examples/page_as_read.rs`, which reads a page the way
//! `fetch_page` would and prints what the model would have been handed, so
//! that a measurement (`scripts/shop-reach.mjs`) judges a page by the
//! extractor itself and not by a copy of it that has drifted.

use crate::tools::{browser, fetch};

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
