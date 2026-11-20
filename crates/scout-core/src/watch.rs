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

use crate::core::blocking;
use crate::store::{NewWatch, PriceWatch, Store, WatchStarted};
use crate::tools::fetch::{FetchArgs, FetchError, FetchPageTool, PageProduct};
use rig::tool::Tool;
use scout_api::ReplyTo;
use std::time::Duration;

/// Live watches an account may hold, and the days one runs before it ends
/// by itself. Both are the design's first guesses, which its owner let
/// stand on 2026-11-20; `/metrics` counts how watches end, and that is
/// what will say whether either is wrong.
pub(crate) const MAX_LIVE: i64 = 10;
pub(crate) const DAYS: i64 = 60;

/// Daily checks in a row that may find no price before the watch ends
/// with a message saying the page can no longer be read.
pub(crate) const UNREADABLE_DAYS: i64 = 3;

/// How long one reading of a page may take. Its parts are bounded already,
/// the plain request at 30 s by the client and a render at 45 s
/// (`RENDER_TIMEOUT`), the wait for a free Chrome included. This is the
/// one bound over both, held here so that it does not rest on theirs.
pub(crate) const READ_BUDGET: Duration = Duration::from_secs(90);

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

/// One reading, given up on after `budget`. A page that never answers is
/// unreadable that day, like one that answered with nothing.
pub(crate) async fn read_within<R: Reader>(reader: &R, url: &str, budget: Duration) -> Reading {
    match tokio::time::timeout(budget, reader.read(url)).await {
        Ok(reading) => reading,
        Err(_) => Reading::Unreadable("it did not answer in time".to_string()),
    }
}

/// An amount as a person reads one: "€329.00". A sign only where it names
/// one currency; any other is written with its code.
pub(crate) fn money(amount: f64, currency: &str) -> String {
    match currency {
        "EUR" => format!("€{amount:.2}"),
        other => format!("{amount:.2} {other}"),
    }
}

/// Whether a price has come down to a target: at it, or under. Decided in
/// cents, the unit a shop prices in, so that what floating point makes of
/// an amount is never the difference.
pub(crate) fn at_or_under(price: f64, target: f64) -> bool {
    let cents = |amount: f64| (amount * 100.0).round() as i64;
    cents(price) <= cents(target)
}

/// What a shop is called to the person: its host, as `fetch_page` names
/// one, without a leading `www.`.
pub(crate) fn shop_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(|host| host.strip_prefix("www.").unwrap_or(host).to_string()))
        .unwrap_or_else(|| "the shop".to_string())
}

/// A day as a message gives one: "16 January". No year: a watch runs
/// sixty days, and the day meant is the next one of that name.
pub(crate) fn day(at: chrono::NaiveDateTime) -> String {
    at.format("%-d %B").to_string()
}

/// What the product is called in a sentence: its name, or for a page whose
/// data gave none, a phrase that stands where a name would.
fn named(title: &str, opens_the_sentence: bool) -> &str {
    match (title.is_empty(), opens_the_sentence) {
        (false, _) => title,
        (true, true) => "This product",
        (true, false) => "this product",
    }
}

/// "under the €300.00" or, to the cent, "exactly the €300.00".
fn against(price: f64, target: f64, currency: &str) -> String {
    let how = if at_or_under(target, price) { "exactly" } else { "under" };
    format!("{how} the {}", money(target, currency))
}

/// A request to be told about a price: whose it is, where to tell them,
/// the page, and the amount they named. The amount is the person's own,
/// as the model heard it. No price of the page's is in here to be wrong.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Ask<'a> {
    pub account_id: i64,
    pub reply_to: &'a ReplyTo,
    pub url: &'a str,
    pub target: f64,
    pub currency: &'a str,
}

/// What asking came to: the watch that is running now, if one is, and the
/// sentence for the person either way. The sentence is written here, with
/// every amount and date in it, so that the model has nothing to work out.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Answer {
    pub started: Option<i64>,
    pub say: String,
}

impl Answer {
    fn not_started(say: String) -> Self {
        Self { started: None, say }
    }
}

/// Starts a watch on a page, if its price can be read and there is
/// anything to wait for.
///
/// The refusals are answers, not errors: each is a sentence for the person
/// that says why. An error is a target that is no amount, which is the
/// caller's mistake, or the store's own failure.
pub(crate) async fn start<R: Reader>(
    store: &Store,
    reader: &R,
    ask: Ask<'_>,
    now: chrono::NaiveDateTime,
) -> anyhow::Result<Answer> {
    if !ask.target.is_finite() || ask.target <= 0.0 {
        anyhow::bail!("target must be an amount above zero, as the user named it");
    }
    let currency = ask.currency.trim().to_uppercase();
    if currency.is_empty() {
        anyhow::bail!("currency must be the ISO code of the target's currency, for example EUR");
    }
    let url = ask.url.trim().to_string();
    let shop = shop_of(&url);

    // Before the page is asked for anything. The store decides both again
    // under its lock when the watch is written; this is what spares a shop
    // a request for a watch that could not start.
    let (theirs, account_id) = (store.clone(), ask.account_id);
    let live = blocking(move || theirs.live_watches(account_id)).await?;
    if let Some(there) = live.iter().find(|watch| watch.url == url) {
        return Ok(Answer::not_started(say_already(there)));
    }
    if live.len() as i64 >= MAX_LIVE {
        return Ok(Answer::not_started(say_full()));
    }

    let read = match read_within(reader, &url, READ_BUDGET).await {
        Reading::Price(read) => read,
        Reading::Unreadable(why) => {
            return Ok(Answer::not_started(format!(
                "I cannot watch this page: {why}. A watch that cannot be checked is not started."
            )))
        }
        Reading::Refused(_) => {
            return Ok(Answer::not_started(format!(
                "I cannot watch this page: {shop} refused Scout's request for it, so there is no price I could \
                 check each day. A watch that cannot be checked is not started."
            )))
        }
    };
    if read.currency != currency {
        return Ok(Answer::not_started(format!(
            "I cannot watch this for an amount in {currency}: the page prices it in {theirs} ({price} now), and I do \
             not compare across currencies. Ask again with the amount in {theirs}.",
            theirs = read.currency,
            price = money(read.amount, &read.currency),
        )));
    }
    let title = read.title.clone().unwrap_or_default();
    if at_or_under(read.amount, ask.target) {
        return Ok(Answer::not_started(format!(
            "{} is already {} at {shop}, {} you asked about, so there is nothing to watch for. {url}",
            named(&title, true),
            money(read.amount, &currency),
            against(read.amount, ask.target, &currency),
        )));
    }

    let expires_at = now + chrono::Duration::days(DAYS);
    let (theirs, reply_to) = (store.clone(), ask.reply_to.clone());
    let (target, price) = (ask.target, read.amount);
    let (kept_url, kept_title, kept_currency) = (url.clone(), title.clone(), currency.clone());
    let started = blocking(move || {
        theirs.start_watch(
            &NewWatch {
                account_id,
                channel: &reply_to.channel,
                address: &reply_to.address,
                url: &kept_url,
                title: &kept_title,
                currency: &kept_currency,
                target,
                price,
                now,
                expires_at,
            },
            MAX_LIVE,
        )
    })
    .await?;
    Ok(match started {
        WatchStarted::Started(watch) => Answer { started: Some(watch.id), say: say_watching(&watch, read.struck_through) },
        WatchStarted::Already(there) => Answer::not_started(say_already(&there)),
        WatchStarted::Full => Answer::not_started(say_full()),
    })
}

/// The first answer. It names the price that was read, and beside it the
/// amount the page's data strikes through, where it has one: on a page
/// that writes a conditional amount as its price, that is the other
/// number the page shows, and the person sees on day one which was read.
fn say_watching(watch: &PriceWatch, struck_through: Option<f64>) -> String {
    let price = money(watch.first_price, &watch.currency);
    let target = money(watch.target, &watch.currency);
    let opening = format!("Watching {} at {}: {price} now", named(&watch.title, false), shop_of(&watch.url));
    let until = day(watch.expires_at);
    match struck_through {
        Some(struck) => format!(
            "{opening}; the page also gives {} as its regular price. I will tell you when it is {target} or less. \
             I check once a day until {until}.",
            money(struck, &watch.currency)
        ),
        None => format!("{opening}, I will tell you when it is {target} or less. I check once a day until {until}."),
    }
}

fn say_already(there: &PriceWatch) -> String {
    format!(
        "I am already watching {} at {} (watch {}) and will tell you when it is {} or less. \
         To change that amount, stop the watch and ask again.",
        named(&there.title, false),
        shop_of(&there.url),
        there.id,
        money(there.target, &there.currency),
    )
}

fn say_full() -> String {
    format!("You have {MAX_LIVE} price watches running, which is as many as I keep at once. Stop one and ask again.")
}

/// A running watch as `list_watches` hands it to the model: every amount
/// and day written out, so that none is the model's to format.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Listed {
    pub id: i64,
    pub title: String,
    pub shop: String,
    pub url: String,
    /// The user is told at this or less.
    pub target: String,
    /// The last price that was read, which is not always the last check's:
    /// see `note`.
    pub last_price: String,
    pub lowest_price: String,
    pub price_when_started: String,
    pub started: String,
    pub last_checked: String,
    pub checked_until: String,
    /// Said only when the page gave no price at the last check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

pub(crate) fn listed(watch: &PriceWatch) -> Listed {
    let amount = |amount: f64| money(amount, &watch.currency);
    Listed {
        id: watch.id,
        title: named(&watch.title, false).to_string(),
        shop: shop_of(&watch.url),
        url: watch.url.clone(),
        target: amount(watch.target),
        last_price: amount(watch.last_price),
        lowest_price: amount(watch.lowest_price),
        price_when_started: amount(watch.first_price),
        started: day(watch.created_at),
        last_checked: day(watch.last_checked_at),
        checked_until: day(watch.expires_at),
        note: (watch.unreadable_streak > 0).then(|| {
            format!(
                "the page gave no price at the last {} of its daily checks; after {UNREADABLE_DAYS} in a row the watch ends",
                watch.unreadable_streak
            )
        }),
    }
}

pub(crate) fn say_stopped(watch: &PriceWatch) -> String {
    format!("Stopped watching {} at {}.", named(&watch.title, false), shop_of(&watch.url))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net_guard::OpenWeb;
    use crate::store::Store;
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

    /// A reader that answers every address with one reading and counts
    /// how often it was asked.
    struct Fixed {
        reading: Reading,
        asked: std::sync::atomic::AtomicUsize,
    }

    impl Fixed {
        fn new(reading: Reading) -> Self {
            Self { reading, asked: std::sync::atomic::AtomicUsize::new(0) }
        }

        fn priced(amount: f64) -> Self {
            Self::new(Reading::Price(Priced { amount, currency: "EUR".into(), title: Some("Widget".into()), struck_through: None }))
        }

        fn asked(&self) -> usize {
            self.asked.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl Reader for Fixed {
        async fn read(&self, _url: &str) -> Reading {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.reading.clone()
        }
    }

    fn a_store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Store::open(dir.path().join("watch.duckdb")).unwrap(), dir)
    }

    /// Now, to the second. Every date below is counted from it.
    fn clock() -> chrono::NaiveDateTime {
        use chrono::Timelike;
        chrono::Utc::now().naive_utc().with_nanosecond(0).unwrap()
    }

    const WIDGET: &str = "https://www.shop.example/p/widget?variant=2";

    fn ask<'a>(reply_to: &'a ReplyTo, url: &'a str, target: f64) -> Ask<'a> {
        Ask { account_id: 1, reply_to, url, target, currency: "EUR" }
    }

    #[tokio::test]
    async fn the_first_answer_names_the_price_that_was_read_the_target_and_the_last_day() {
        let (store, _dir) = a_store();
        let now = clock();
        let chat = ReplyTo::telegram(4242);
        let answer = start(&store, &Fixed::priced(329.0), ask(&chat, WIDGET, 300.0), now).await.unwrap();

        let until = (now + chrono::Duration::days(60)).format("%-d %B");
        assert_eq!(
            answer.say,
            format!(
                "Watching Widget at shop.example: €329.00 now, I will tell you when it is €300.00 or less. \
                 I check once a day until {until}."
            )
        );
        let watch = &store.live_watches(1).unwrap()[0];
        assert_eq!(answer.started, Some(watch.id));
        assert_eq!((watch.first_price, watch.target, watch.currency.as_str()), (329.0, 300.0, "EUR"));
        assert_eq!((watch.url.as_str(), watch.title.as_str()), (WIDGET, "Widget"));
        assert_eq!((watch.created_at, watch.expires_at), (now, now + chrono::Duration::days(60)));
        assert_eq!((watch.channel.as_str(), watch.address.as_str()), ("telegram", "4242"));
    }

    #[tokio::test]
    async fn the_amount_the_page_strikes_through_is_said_beside_the_price_and_not_kept() {
        // Where a shop writes a conditional amount as its price, the one
        // it struck through beside it is what gives that away on day one.
        let (store, _dir) = a_store();
        let now = clock();
        let chat = ReplyTo::telegram(4242);
        let read = Fixed::new(Reading::Price(Priced {
            amount: 30.59,
            currency: "EUR".into(),
            title: Some("Widget 12 kg".into()),
            struck_through: Some(35.99),
        }));
        let answer = start(&store, &read, ask(&chat, WIDGET, 28.0), now).await.unwrap();
        let until = (now + chrono::Duration::days(60)).format("%-d %B");
        assert_eq!(
            answer.say,
            format!(
                "Watching Widget 12 kg at shop.example: €30.59 now; the page also gives €35.99 as its regular price. \
                 I will tell you when it is €28.00 or less. I check once a day until {until}."
            )
        );
        let watch = &store.live_watches(1).unwrap()[0];
        assert_eq!((watch.first_price, watch.last_price, watch.lowest_price), (30.59, 30.59, 30.59));
    }

    #[tokio::test]
    async fn a_watch_is_started_only_on_a_page_whose_price_was_read() {
        let chat = ReplyTo::telegram(4242);
        for (reading, said) in [
            (Reading::Unreadable("its data states no product".into()), "I cannot watch this page: its data states no product."),
            (Reading::Unreadable("page returned HTTP 404 Not Found".into()), "I cannot watch this page: page returned HTTP 404 Not Found."),
            (
                Reading::Refused("shop.example served a block notice instead of the page".into()),
                "I cannot watch this page: shop.example refused Scout's request for it, so there is no price I could check each day.",
            ),
        ] {
            let (store, _dir) = a_store();
            let answer = start(&store, &Fixed::new(reading), ask(&chat, WIDGET, 300.0), clock()).await.unwrap();
            assert_eq!(answer.started, None);
            assert!(answer.say.starts_with(said), "got: {}", answer.say);
            assert!(answer.say.ends_with("A watch that cannot be checked is not started."), "got: {}", answer.say);
            assert!(store.live_watches(1).unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn a_target_in_another_currency_than_the_pages_starts_nothing() {
        let (store, _dir) = a_store();
        let chat = ReplyTo::telegram(4242);
        let dollars = Ask { currency: "usd", ..ask(&chat, WIDGET, 300.0) };
        let answer = start(&store, &Fixed::priced(329.0), dollars, clock()).await.unwrap();
        assert_eq!(answer.started, None);
        assert_eq!(
            answer.say,
            "I cannot watch this for an amount in USD: the page prices it in EUR (€329.00 now), and I do not compare across \
             currencies. Ask again with the amount in EUR."
        );
        assert!(store.live_watches(1).unwrap().is_empty());
        // The same currency however it is written.
        let euros = Ask { currency: " eur ", ..ask(&chat, WIDGET, 300.0) };
        assert!(start(&store, &Fixed::priced(329.0), euros, clock()).await.unwrap().started.is_some());
    }

    #[tokio::test]
    async fn a_price_already_at_or_under_the_target_is_said_instead_of_watched() {
        let chat = ReplyTo::telegram(4242);
        for (price, said) in [
            (289.0, "Widget is already €289.00 at shop.example, under the €300.00 you asked about, so there is nothing to watch for."),
            (300.0, "Widget is already €300.00 at shop.example, exactly the €300.00 you asked about, so there is nothing to watch for."),
        ] {
            let (store, _dir) = a_store();
            let answer = start(&store, &Fixed::priced(price), ask(&chat, WIDGET, 300.0), clock()).await.unwrap();
            assert_eq!(answer.started, None);
            assert_eq!(answer.say, format!("{said} {WIDGET}"));
            assert!(store.live_watches(1).unwrap().is_empty());
        }
        // A cent over is not there yet.
        let (store, _dir) = a_store();
        assert!(start(&store, &Fixed::priced(300.01), ask(&chat, WIDGET, 300.0), clock()).await.unwrap().started.is_some());
    }

    #[tokio::test]
    async fn ten_live_watches_is_the_cap_and_the_eleventh_page_is_not_even_read() {
        let (store, _dir) = a_store();
        let chat = ReplyTo::telegram(4242);
        let read = Fixed::priced(329.0);
        for n in 0..10 {
            let url = format!("https://shop.example/p/{n}");
            assert!(start(&store, &read, ask(&chat, &url, 300.0), clock()).await.unwrap().started.is_some(), "watch {n}");
        }
        let answer = start(&store, &read, ask(&chat, "https://shop.example/p/10", 300.0), clock()).await.unwrap();
        assert_eq!(answer.started, None);
        assert_eq!(answer.say, "You have 10 price watches running, which is as many as I keep at once. Stop one and ask again.");
        assert_eq!(store.live_watches(1).unwrap().len(), 10);
        assert_eq!(read.asked(), 10, "a page is not read for a watch that cannot start");

        // Another account's ten are not this one's.
        let theirs = Ask { account_id: 2, ..ask(&chat, "https://shop.example/p/10", 300.0) };
        assert!(start(&store, &read, theirs, clock()).await.unwrap().started.is_some());
    }

    #[tokio::test]
    async fn asking_twice_for_one_address_leaves_one_watch_and_says_which() {
        let (store, _dir) = a_store();
        let chat = ReplyTo::telegram(4242);
        let read = Fixed::priced(329.0);
        let first = start(&store, &read, ask(&chat, WIDGET, 300.0), clock()).await.unwrap().started.unwrap();
        let again = start(&store, &read, ask(&chat, &format!(" {WIDGET} "), 280.0), clock()).await.unwrap();
        assert_eq!(again.started, None);
        assert_eq!(
            again.say,
            format!(
                "I am already watching Widget at shop.example (watch {first}) and will tell you when it is €300.00 or less. \
                 To change that amount, stop the watch and ask again."
            )
        );
        assert_eq!(store.live_watches(1).unwrap().len(), 1);
        assert_eq!(read.asked(), 1);
    }

    #[tokio::test]
    async fn a_target_that_is_not_an_amount_is_the_callers_mistake() {
        let (store, _dir) = a_store();
        let chat = ReplyTo::telegram(4242);
        let read = Fixed::priced(329.0);
        for target in [0.0, -5.0, f64::NAN, f64::INFINITY] {
            let err = start(&store, &read, ask(&chat, WIDGET, target), clock()).await.unwrap_err();
            assert!(err.to_string().contains("target"), "{target}: {err}");
        }
        let unnamed = Ask { currency: " ", ..ask(&chat, WIDGET, 300.0) };
        assert!(start(&store, &read, unnamed, clock()).await.unwrap_err().to_string().contains("currency"));
        assert_eq!(read.asked(), 0);
    }

    #[tokio::test]
    async fn a_page_that_names_no_product_is_watched_as_this_product() {
        let (store, _dir) = a_store();
        let chat = ReplyTo::telegram(4242);
        let nameless = Fixed::new(Reading::Price(Priced { amount: 329.0, currency: "EUR".into(), title: None, struck_through: None }));
        let answer = start(&store, &nameless, ask(&chat, WIDGET, 300.0), clock()).await.unwrap();
        assert!(answer.say.starts_with("Watching this product at shop.example: €329.00 now"), "got: {}", answer.say);
    }

    #[tokio::test(start_paused = true)]
    async fn a_page_that_never_answers_is_given_up_on_and_nothing_is_started() {
        struct Silent;
        impl Reader for Silent {
            async fn read(&self, _url: &str) -> Reading {
                std::future::pending().await
            }
        }
        let (store, _dir) = a_store();
        let chat = ReplyTo::telegram(4242);
        let began = tokio::time::Instant::now();
        let answer = start(&store, &Silent, ask(&chat, WIDGET, 300.0), clock()).await.unwrap();
        assert_eq!(began.elapsed(), READ_BUDGET);
        assert_eq!(answer.started, None);
        assert!(answer.say.starts_with("I cannot watch this page: it did not answer in time."), "got: {}", answer.say);
    }

    #[tokio::test]
    async fn a_listed_watch_has_its_amounts_and_days_written_out() {
        let (store, _dir) = a_store();
        let now = clock();
        let chat = ReplyTo::telegram(4242);
        let id = start(&store, &Fixed::priced(329.0), ask(&chat, WIDGET, 300.0), now).await.unwrap().started.unwrap();
        let later = now + chrono::Duration::days(2);
        let check = crate::store::WatchCheck { id, now: later, last_price: 310.0, lowest_price: 310.0, unreadable_streak: 0, ended: None };
        assert!(store.record_watch_check(&check).unwrap());

        let row = listed(&store.live_watches(1).unwrap()[0]);
        assert_eq!((row.id, row.title.as_str(), row.shop.as_str(), row.url.as_str()), (id, "Widget", "shop.example", WIDGET));
        assert_eq!((row.target.as_str(), row.last_price.as_str(), row.lowest_price.as_str()), ("€300.00", "€310.00", "€310.00"));
        assert_eq!(row.price_when_started, "€329.00");
        assert_eq!(row.started, now.format("%-d %B").to_string());
        assert_eq!(row.last_checked, later.format("%-d %B").to_string());
        assert_eq!(row.checked_until, (now + chrono::Duration::days(60)).format("%-d %B").to_string());
        assert_eq!(row.note, None);

        // A check that read nothing leaves the price as it was, and the
        // row says the price is older than the check.
        assert!(store.record_watch_check(&crate::store::WatchCheck { unreadable_streak: 2, ..check }).unwrap());
        let row = listed(&store.live_watches(1).unwrap()[0]);
        assert_eq!(row.last_price, "€310.00");
        assert_eq!(
            row.note.as_deref(),
            Some("the page gave no price at the last 2 of its daily checks; after 3 in a row the watch ends")
        );
    }

    #[test]
    fn an_amount_is_written_as_a_person_reads_one() {
        assert_eq!(money(329.0, "EUR"), "€329.00");
        assert_eq!(money(5.5, "EUR"), "€5.50");
        assert_eq!(money(1299.999, "EUR"), "€1300.00");
        // A sign only where it names one currency.
        assert_eq!(money(329.0, "USD"), "329.00 USD");
        assert_eq!(money(89.9, "SEK"), "89.90 SEK");
    }

    #[test]
    fn at_or_under_is_decided_in_cents() {
        assert!(at_or_under(300.0, 300.0) && at_or_under(299.99, 300.0));
        assert!(!at_or_under(300.01, 300.0));
        // What floating point makes of an amount is not a cent's difference.
        assert!(at_or_under(0.1 + 0.2, 0.3));
        assert!(at_or_under(300.004, 300.0) && !at_or_under(300.006, 300.0));
    }

    #[test]
    fn a_shop_is_named_by_its_host() {
        assert_eq!(shop_of("https://www.bol.com/nl/nl/p/x/123/"), "bol.com");
        assert_eq!(shop_of("https://shop.example/p/widget?variant=2"), "shop.example");
        assert_eq!(shop_of("not an address"), "the shop");
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
