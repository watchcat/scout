//! A price somebody asked to be told about.
//!
//! "Tell me when it is under 300." A watch is started on a page whose price
//! Rust can read, and that page is read again once a day until the price is
//! at or under what was asked, the page has given no price three days
//! running, or sixty days are up. Each of those ends the watch with one
//! message. No model is called to check a watch, and none ever supplies a
//! price: the price read, the comparison and every amount in a message are
//! this file's.
//!
//! What a watch reads is `fetch_page`'s product block: the page's own
//! machine-readable price for the address the watch was started on. Where
//! a page states a "from" price there, or a marketplace seller's, that is
//! the number watched; the first answer names the price that was read, so
//! a wrong one is visible on the day the watch starts.

use crate::tools::fetch::{FetchArgs, FetchError, FetchPageTool, PageProduct};
use rig::tool::Tool;

/// The longest name of a product a watch keeps. It is said back in every
/// message about the watch, and a shop's name for a thing can be a
/// paragraph.
const MAX_TITLE_CHARS: usize = 120;

/// What reading a watched page came to.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Reading {
    Price(Priced),
    /// The page was asked for and no price came of it: it is gone, its data
    /// prices nothing, or the request failed. The words say which, and
    /// finish the sentence "this page cannot be watched: …".
    Unreadable(String),
    /// The site refused its reader, with a block notice or a bot check.
    /// Not the same thing: nothing was learnt of the page, which may be
    /// priced as it ever was.
    Refused(String),
}

/// A price as the page's own data states it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Priced {
    pub amount: f64,
    pub currency: String,
    /// The product's name, where the data gives one.
    pub title: Option<String>,
    /// What the data gives beside the price as the regular or former one.
    /// Said once, in the first answer, and neither kept nor compared.
    pub struck_through: Option<f64>,
}

/// Something that reads a watched page. The daily pass is written against
/// this and not against the network, so that its spacing and its endings
/// can be tested on a clock the test holds.
pub(crate) trait Reader {
    async fn read(&self, url: &str) -> Reading;
}

/// The page read the way `fetch_page` reads one: the plain request, then
/// the render where one is worth it, both behind the guard. There is no
/// second fetcher, so a watch reads what the model was shown when it
/// opened the same address.
pub(crate) struct PageReader {
    tool: FetchPageTool,
}

impl PageReader {
    /// For one `watch_price` call: a reader of its own, so the price a
    /// watch starts from is read then and not remembered from earlier.
    pub(crate) fn for_a_request(web: crate::net_guard::OpenWeb, renderer: Option<crate::tools::browser::Renderer>) -> Self {
        Self { tool: FetchPageTool::new(web, renderer) }
    }

    /// For one daily pass over every watch that is due; see
    /// `FetchPageTool::for_a_pass` for what it keeps and what it is not
    /// held to.
    pub(crate) fn for_a_pass(web: crate::net_guard::OpenWeb, renderer: Option<crate::tools::browser::Renderer>) -> Self {
        Self { tool: FetchPageTool::for_a_pass(web, renderer) }
    }
}

impl Reader for PageReader {
    async fn read(&self, url: &str) -> Reading {
        match self.tool.call(FetchArgs { url: url.to_string() }).await {
            Ok(page) => priced(page.product),
            Err(FetchError::Refused(said)) => Reading::Refused(said),
            Err(e) => Reading::Unreadable(e.to_string()),
        }
    }
}

/// The product block as a price to watch. A price needs its currency:
/// Scout does not compare across currencies, and an amount with none
/// cannot be set against the one the person named.
fn priced(product: Option<PageProduct>) -> Reading {
    let Some(product) = product else {
        return Reading::Unreadable("its data states no product".to_string());
    };
    let Some(stated) = product.price.as_deref() else {
        return Reading::Unreadable("its data states no price for the product".to_string());
    };
    let Some((amount, currency)) = stated.split_once(' ') else {
        return Reading::Unreadable("its data states a price and no currency".to_string());
    };
    // `fetch_page` writes every price as "13.80 EUR", so anything else is
    // a block this file does not know how to read, and is not guessed at.
    let amount = match amount.parse::<f64>() {
        Ok(amount) if amount.is_finite() && amount > 0.0 => amount,
        _ => return Reading::Unreadable("its data states a price that is not an amount".to_string()),
    };
    let struck_through = product
        .struck_through_price
        .as_deref()
        .and_then(|stated| stated.split_once(' '))
        .filter(|(_, theirs)| *theirs == currency)
        .and_then(|(amount, _)| amount.parse::<f64>().ok())
        .filter(|amount| amount.is_finite() && *amount > 0.0);
    Reading::Price(Priced {
        amount,
        currency: currency.to_uppercase(),
        title: product.name.as_deref().map(tidy_title).filter(|title| !title.is_empty()),
        struck_through,
    })
}

/// A product's name as a watch keeps it: one line, and no longer than a
/// name. It is the page's text, and goes into messages to a person.
fn tidy_title(name: &str) -> String {
    let one_line = name.split_whitespace().collect::<Vec<_>>().join(" ");
    one_line.chars().take(MAX_TITLE_CHARS).collect::<String>().trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net_guard::OpenWeb;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A product page priced in its data at `offer`, the inside of a
    /// schema.org Offer, with other amounts in its text as a shop's page
    /// has them.
    fn product_page(name: &str, offer: &str) -> String {
        format!(
            r#"<html><head><title>{name}</title><script type="application/ld+json">
               {{"@type":"Product","name":"{name}","offers":{{"@type":"Offer",{offer}}}}}</script></head>
               <body><h1>{name}</h1><p>Others bought: 12,99 and 299,00</p><p>{}</p></body></html>"#,
            "A widget for every purpose. ".repeat(100)
        )
    }

    fn widget_at(price: &str) -> String {
        product_page("Widget", &format!(r#""price":"{price}","priceCurrency":"EUR""#))
    }

    /// A shop's notice that it will not serve this reader, in the words
    /// `refusal_marks.txt` knows one by.
    const NOTICE: &str = "<html><head><title>shop</title></head><body><h1>Access blocked</h1>\
        <p>Your access has been temporarily blocked.</p></body></html>";

    async fn serving(status: u16, body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(status).set_body_string(body)).mount(&server).await;
        server
    }

    fn reader(server: &MockServer) -> PageReader {
        PageReader::for_a_request(OpenWeb::exempting(*server.address()), None)
    }

    async fn read(server: &MockServer) -> Reading {
        reader(server).read(&format!("{}/p/widget", server.uri())).await
    }

    #[tokio::test]
    async fn a_price_is_read_from_the_pages_own_data_and_never_from_its_text() {
        let server = serving(200, &widget_at("329.00")).await;
        assert_eq!(
            read(&server).await,
            Reading::Price(Priced { amount: 329.0, currency: "EUR".into(), title: Some("Widget".into()), struck_through: None })
        );

        // With amounts all over its text and none in its data, a page has
        // no price to watch: the text does not say which is the product's.
        let unpriced = "<html><body><h1>Widget</h1><p>Now 299,00, was 329,00</p></body></html>".to_string()
            + &"<p>A widget for every purpose.</p>".repeat(100);
        let server = serving(200, &unpriced).await;
        match read(&server).await {
            Reading::Unreadable(why) => assert_eq!(why, "its data states no product"),
            other => panic!("a price out of the text: {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_amount_the_data_strikes_through_is_read_beside_the_price() {
        let offer = r#""price":30.59,"priceCurrency":"EUR","priceSpecification":
            {"priceType":"https://schema.org/StrikethroughPrice","price":35.99,"priceCurrency":"EUR"}"#;
        let server = serving(200, &product_page("Widget 12 kg", offer)).await;
        match read(&server).await {
            Reading::Price(read) => {
                assert_eq!((read.amount, read.struck_through), (30.59, Some(35.99)));
                assert_eq!(read.title.as_deref(), Some("Widget 12 kg"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_price_with_no_currency_or_no_amount_is_not_one_to_watch() {
        for (offer, why) in [
            (r#""price":"329.00""#, "its data states a price and no currency"),
            (r#""price":"on request","priceCurrency":"EUR""#, "its data states no price for the product"),
        ] {
            let server = serving(200, &product_page("Widget", offer)).await;
            assert_eq!(read(&server).await, Reading::Unreadable(why.to_string()), "with {offer}");
        }
    }

    #[tokio::test]
    async fn a_page_that_is_gone_is_unreadable_and_a_site_that_says_no_has_refused() {
        // The two are kept apart from here on: a page that lost its price
        // counts towards ending a watch, and a shop that walls the reader
        // for a day says nothing about the page.
        for status in [404, 410, 500] {
            let server = serving(status, "").await;
            match read(&server).await {
                Reading::Unreadable(why) => assert!(why.contains(&format!("HTTP {status}")), "got: {why}"),
                other => panic!("{status}: {other:?}"),
            }
        }
        let challenge = "<html><head><title>Just a moment...</title></head><body></body></html>";
        for wall in [NOTICE, challenge] {
            let server = serving(200, wall).await;
            assert!(matches!(read(&server).await, Reading::Refused(_)), "for {wall}");
        }
    }

    #[tokio::test]
    async fn an_address_that_is_not_public_is_refused_by_the_guard_and_never_opened() {
        // The address is the model's word. A watch must not be a way to
        // have Scout ask the pod's own listeners something once a day.
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_string(widget_at("1.00"))).expect(0).mount(&server).await;
        let guarded = PageReader::for_a_request(OpenWeb::new(std::time::Duration::from_secs(5)).unwrap(), None);
        for url in [format!("{}/metrics", server.uri()), "http://10.43.0.1/".to_string(), "ftp://shop.example/p".to_string()] {
            match guarded.read(&url).await {
                Reading::Unreadable(why) => assert!(why.contains("not on the public internet") || why.contains("scheme"), "{url}: {why}"),
                other => panic!("{url}: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn the_address_is_read_as_it_was_given_query_and_all() {
        // A shop that keeps its variants at one path tells them apart by
        // the query, and so does the product block.
        let variants = r#"<html><head><script type="application/ld+json">
            {"@type":"ProductGroup","name":"Widget","hasVariant":[
             {"@type":"Product","name":"Widget small","url":"/p/widget?variant=1","offers":{"@type":"Offer","price":"5.00","priceCurrency":"EUR"}},
             {"@type":"Product","name":"Widget large","url":"/p/widget?variant=2","offers":{"@type":"Offer","price":"8.00","priceCurrency":"EUR"}}]}
            </script></head><body>"#
            .to_string()
            + &"<p>A widget for every purpose.</p>".repeat(100)
            + "</body></html>";
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/p/widget")).and(query_param("variant", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_string(variants)).expect(1).mount(&server).await;
        match reader(&server).read(&format!("{}/p/widget?variant=2", server.uri())).await {
            Reading::Price(read) => assert_eq!((read.title.as_deref(), read.amount), (Some("Widget large"), 8.0)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_products_name_is_kept_as_one_line_of_a_names_length() {
        assert_eq!(tidy_title("  Widget\n   12 kg\t(blue) "), "Widget 12 kg (blue)");
        let long = "Widget ".repeat(60);
        let kept = tidy_title(&long);
        assert!(kept.chars().count() <= MAX_TITLE_CHARS && kept.starts_with("Widget Widget"), "got: {kept}");
        assert!(!kept.ends_with(' '));
        // A name that is nothing but white space is no name.
        let nameless = PageProduct { name: Some(" \n ".into()), price: Some("9.99 EUR".into()), struck_through_price: None, availability: None, seller: None };
        assert!(matches!(priced(Some(nameless)), Reading::Price(Priced { title: None, .. })));
    }
}
