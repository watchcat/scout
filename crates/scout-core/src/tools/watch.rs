use super::purchases::{internal, StoreToolError};
use crate::store::Store;
use crate::watch::{self, Ask, Listed, PageReader};
use rig::tool::Tool;
use scout_api::ReplyTo;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// What `watch_price` is called with. There is no price of the page's in
/// it: the price a watch starts from is the one Rust reads, and a field
/// the model could fill is a price the model could invent.
#[derive(Deserialize)]
pub struct WatchPriceArgs {
    pub url: String,
    pub target: f64,
    pub currency: String,
}

/// What `watch_price` answers: whether a watch is running now, and the
/// sentence for the user either way, amounts and date written in Rust.
#[derive(Debug, Serialize)]
pub struct WatchAnswer {
    pub watching: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub say: String,
}

/// Pages `watch_price` may read in one request. It reads a page as
/// `fetch_page` does, a render behind it where one is wanted, and a tool
/// with no bound of its own was a way round that tool's five: a price and
/// a name for any page, as often as a model asked. Three because a person
/// asking to be told about more than three things in one message has not
/// been seen; not measured.
const MAX_READS: usize = 3;

pub struct WatchPriceTool {
    store: Store,
    account_id: i64,
    /// Where the watch's one message will go: the chat this run is in.
    /// Carried, as a reminder's is, because in a group that is the group.
    reply_to: ReplyTo,
    /// One reader a request, as `fetch_page` has one: what it learns of a
    /// host that refuses lasts the request.
    reader: PageReader,
    /// The request's memo of its shops' pages, which `fetch_page` writes
    /// and the delivery desk reads. A shop that refused either of them is
    /// not asked again here, and one that refuses here is noted there.
    shops: std::sync::Arc<super::delivery::ShopPages>,
    reads: std::sync::atomic::AtomicUsize,
}

impl WatchPriceTool {
    pub fn new(
        store: Store,
        account_id: i64,
        reply_to: ReplyTo,
        web: crate::net_guard::OpenWeb,
        renderer: Option<super::browser::Renderer>,
        shops: std::sync::Arc<super::delivery::ShopPages>,
    ) -> Self {
        Self {
            store,
            account_id,
            reply_to,
            reader: PageReader::for_a_request(web, renderer, shops.clone()),
            shops,
            reads: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

/// The request's reader as one call to `watch_price` may use it: not for a
/// shop the request already knows to refuse, and counted when it is used.
struct Metered<'a> {
    tool: &'a WatchPriceTool,
}

impl watch::Reader for Metered<'_> {
    async fn read(&self, url: &str) -> watch::Reading {
        let host = reqwest::Url::parse(url).ok().as_ref().and_then(super::fetch::host_key);
        if host.is_some_and(|host| self.tool.shops.closed(&host)) {
            return watch::Reading::Refused;
        }
        self.tool.reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.tool.reader.read(url).await
    }
}

impl Tool for WatchPriceTool {
    const NAME: &'static str = "watch_price";
    type Error = StoreToolError;
    type Args = WatchPriceArgs;
    type Output = WatchAnswer;

    fn description(&self) -> String {
        format!(
            "Start a daily price watch on one product page, for a request like 'tell me when this is \
             under 300 euros'. Scout reads the page's current price itself, reads it again once a day, \
             and sends the user one message when it is at or below the target; a watch ends by itself \
             after {} days. Pass the product page's exact URL as tool output gave it (the variant's \
             own address where it has one), the amount the user named, and that amount's currency. \
             Never pass or state a current price of your own. The result's `say` is the answer for the \
             user: give it with every amount and date exactly as written. When `watching` is false no \
             watch was started and `say` is why. It reads at most {MAX_READS} pages a request.",
            watch::DAYS
        )
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "the product page to watch, copied from tool output"},
                "target": {"type": "number", "description": "the amount the user asked to be told at or under"},
                "currency": {"type": "string", "description": "ISO code of the target's currency, e.g. EUR"}
            },
            "required": ["url", "target", "currency"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        // Before anything is asked of anyone. A call that reads no page,
        // one for an address already watched, is refused here too once the
        // three are spent: the bound is on what the tool may cost.
        if self.reads.load(std::sync::atomic::Ordering::Relaxed) >= MAX_READS {
            return Err(StoreToolError(format!(
                "watch_price has read {MAX_READS} pages in this request, which is as many as it may — do not call it \
                 again now; tell the user which watches were started and that the rest can be asked for in a new message"
            )));
        }
        let ask = Ask {
            account_id: self.account_id,
            reply_to: &self.reply_to,
            url: &args.url,
            target: args.target,
            currency: &args.currency,
        };
        let answer = watch::start(&self.store, &Metered { tool: self }, ask, chrono::Utc::now().naive_utc()).await.map_err(internal)?;
        Ok(WatchAnswer { watching: answer.started.is_some(), id: answer.started, say: answer.say })
    }
}

pub struct ListWatchesTool {
    pub store: Store,
    pub account_id: i64,
}

#[derive(Deserialize)]
pub struct NoArgs {}

impl Tool for ListWatchesTool {
    const NAME: &'static str = "list_watches";
    type Error = StoreToolError;
    type Args = NoArgs;
    type Output = Vec<Listed>;

    fn description(&self) -> String {
        "List the user's running price watches: what is watched and where, the target, the last \
         price Scout read and the day it last checked. The amounts and days are written out; \
         repeat them as they are."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
        let (store, account_id) = (self.store.clone(), self.account_id);
        let live = tokio::task::spawn_blocking(move || store.live_watches(account_id))
            .await
            .map_err(internal)?
            .map_err(internal)?;
        Ok(live.iter().map(watch::listed).collect())
    }
}

#[derive(Deserialize)]
pub struct StopWatchArgs {
    pub id: i64,
}

pub struct StopWatchTool {
    pub store: Store,
    pub account_id: i64,
    /// Where a stop is counted, beside the endings the daily pass counts.
    pub metrics: &'static crate::metrics::Metrics,
}

impl Tool for StopWatchTool {
    const NAME: &'static str = "stop_watch";
    type Error = StoreToolError;
    type Args = StopWatchArgs;
    type Output = String;

    fn description(&self) -> String {
        "Stop one of the user's price watches by id (see list_watches). Nothing more is sent about \
         a stopped watch."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {"id": {"type": "integer", "description": "the watch's id"}},
            "required": ["id"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let (store, account_id) = (self.store.clone(), self.account_id);
        let now = chrono::Utc::now().naive_utc();
        let stopped = tokio::task::spawn_blocking(move || store.stop_watch(account_id, args.id, now))
            .await
            .map_err(internal)?
            .map_err(internal)?;
        Ok(match stopped {
            Some(watch) => {
                self.metrics.watch_ended(watch::STOPPED);
                watch::say_stopped(&watch)
            }
            None => "no running price watch with that id; list_watches shows the ones there are".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net_guard::OpenWeb;
    use crate::tools::delivery::ShopPages;
    use std::sync::Arc;
    use tempfile::TempDir;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn setup() -> (Store, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("t.duckdb")).unwrap();
        (store, dir)
    }

    /// A shop serving one product page, priced at 329.00 EUR in its data.
    async fn a_shop() -> MockServer {
        let page = format!(
            r#"<html><head><title>Widget</title><script type="application/ld+json">
               {{"@type":"Product","name":"Widget","offers":{{"@type":"Offer","price":"329.00","priceCurrency":"EUR"}}}}
               </script></head><body><h1>Widget</h1><p>{}</p></body></html>"#,
            "A widget for every purpose. ".repeat(100)
        );
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_string(page)).mount(&server).await;
        server
    }

    fn watching(store: &Store, shop: &MockServer, account_id: i64, reply_to: ReplyTo) -> WatchPriceTool {
        watching_in(store, shop, account_id, reply_to, Arc::default())
    }

    /// The tool as one request builds it, with that request's memo of
    /// what its shops' pages said.
    fn watching_in(store: &Store, shop: &MockServer, account_id: i64, reply_to: ReplyTo, shops: Arc<ShopPages>) -> WatchPriceTool {
        WatchPriceTool::new(store.clone(), account_id, reply_to, OpenWeb::exempting(*shop.address()), None, shops)
    }

    async fn asked(shop: &MockServer) -> usize {
        shop.received_requests().await.expect("the mock records what it is asked").len()
    }

    /// A shop that answers every page with its notice that it will not
    /// serve this reader.
    async fn a_shop_that_refuses() -> MockServer {
        let notice = "<html><head><title>shop</title></head><body><h1>Access blocked</h1>\
            <p>Your access has been temporarily blocked.</p></body></html>";
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_string(notice)).mount(&server).await;
        server
    }

    /// A set of counters of the test's own.
    fn counters() -> &'static crate::metrics::Metrics {
        Box::leak(Box::default())
    }

    fn under(shop: &MockServer, page: &str, target: f64) -> WatchPriceArgs {
        WatchPriceArgs { url: format!("{}/p/{page}", shop.uri()), target, currency: "EUR".into() }
    }

    #[tokio::test]
    async fn a_watch_is_started_listed_and_stopped_in_conversation() {
        let (store, _d) = setup();
        let shop = a_shop().await;
        let started = watching(&store, &shop, 1, ReplyTo::telegram(99)).call(under(&shop, "widget", 300.0)).await.unwrap();
        assert!(started.watching);
        assert!(
            started.say.starts_with("Watching Widget at 127.0.0.1: €329.00 now, I will tell you when it is €300.00 or less."),
            "got: {}",
            started.say
        );

        let list = ListWatchesTool { store: store.clone(), account_id: 1 };
        let listed = list.call(NoArgs {}).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(Some(listed[0].id), started.id);
        assert_eq!((listed[0].title.as_str(), listed[0].shop.as_str()), ("Widget", "127.0.0.1"));
        assert_eq!((listed[0].last_price.as_str(), listed[0].target.as_str()), ("€329.00", "€300.00"));
        assert_eq!(listed[0].url, format!("{}/p/widget", shop.uri()));
        // What the model is handed is written out, not left for it to format.
        let handed = serde_json::to_value(&listed).unwrap();
        assert_eq!(handed[0]["last_price"], "€329.00");
        assert!(handed[0].get("note").is_none(), "nothing to remark on: {handed}");

        let stop = StopWatchTool { store: store.clone(), account_id: 1, metrics: counters() };
        assert_eq!(stop.call(StopWatchArgs { id: listed[0].id }).await.unwrap(), "Stopped watching Widget at 127.0.0.1.");
        assert!(list.call(NoArgs {}).await.unwrap().is_empty());
        let again = stop.call(StopWatchArgs { id: listed[0].id }).await.unwrap();
        assert!(again.starts_with("no running price watch with that id"), "got: {again}");
    }

    #[tokio::test]
    async fn a_watch_is_its_accounts_own_to_list_and_to_stop() {
        let (store, _d) = setup();
        let shop = a_shop().await;
        let mine = watching(&store, &shop, 1, ReplyTo::telegram(99)).call(under(&shop, "widget", 300.0)).await.unwrap();
        assert!(ListWatchesTool { store: store.clone(), account_id: 2 }.call(NoArgs {}).await.unwrap().is_empty());
        let theirs = StopWatchTool { store: store.clone(), account_id: 2, metrics: counters() };
        let said = theirs.call(StopWatchArgs { id: mine.id.unwrap() }).await.unwrap();
        assert!(said.starts_with("no running price watch with that id"), "got: {said}");
        assert_eq!(ListWatchesTool { store, account_id: 1 }.call(NoArgs {}).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_watch_asked_for_in_a_group_is_the_groups_to_hear_about() {
        // The rule a reminder follows, and for its reason: the chat the
        // run is in is carried, not looked up from where the account last
        // spoke, which by the day of the message can be somewhere else.
        let (store, _d) = setup();
        let shop = a_shop().await;
        let group = ReplyTo::telegram(-100_123);
        let started = watching(&store, &shop, 1, group).call(under(&shop, "widget", 300.0)).await.unwrap();
        let watch = store.watch_for_tests(started.id.unwrap()).unwrap().unwrap();
        assert_eq!((watch.channel.as_str(), watch.address.as_str()), ("telegram", "-100123"));
    }

    #[tokio::test]
    async fn the_model_cannot_supply_the_price_a_watch_starts_from() {
        let (store, _d) = setup();
        let shop = a_shop().await;
        let tool = watching(&store, &shop, 1, ReplyTo::telegram(99));
        let offered = tool.parameters();
        let mut names: Vec<&str> = offered["properties"].as_object().unwrap().keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["currency", "target", "url"], "an address and what the user named, and nothing of the page's");

        // And one handed over all the same goes nowhere.
        let args: WatchPriceArgs = serde_json::from_value(json!({
            "url": format!("{}/p/widget", shop.uri()), "target": 300, "currency": "EUR",
            "price": 301.0, "current_price": 301.0, "first_price": 301.0
        }))
        .unwrap();
        let started = tool.call(args).await.unwrap();
        assert!(started.say.contains("€329.00 now"), "got: {}", started.say);
        assert_eq!(store.watch_for_tests(started.id.unwrap()).unwrap().unwrap().first_price, 329.0);
    }

    #[tokio::test]
    async fn a_refusal_is_an_answer_with_its_reason_and_not_a_failed_tool() {
        // A tool that errs is counted as failed and its words reach the
        // model as an error. "It is already under that" is neither.
        let (store, _d) = setup();
        let shop = a_shop().await;
        let tool = watching(&store, &shop, 1, ReplyTo::telegram(99));
        let already = tool.call(under(&shop, "widget", 400.0)).await.unwrap();
        assert!(!already.watching && already.id.is_none());
        assert!(already.say.starts_with("Widget is already €329.00 at 127.0.0.1, under the €400.00 you asked about"), "got: {}", already.say);
        let handed = serde_json::to_value(&already).unwrap();
        assert_eq!(handed["watching"], false);
        assert!(handed.get("id").is_none(), "got: {handed}");

        // What is the caller's own mistake is an error it can correct.
        let err = tool.call(under(&shop, "widget", -1.0)).await.unwrap_err();
        assert!(err.to_string().contains("target"), "got: {err}");
    }

    #[tokio::test]
    async fn a_watch_its_owner_stops_is_counted_as_stopped_and_a_stop_that_found_none_is_not() {
        let (store, _d) = setup();
        let shop = a_shop().await;
        let started = watching(&store, &shop, 1, ReplyTo::telegram(99)).call(under(&shop, "widget", 300.0)).await.unwrap();
        let metrics = counters();
        let stop = StopWatchTool { store: store.clone(), account_id: 1, metrics };
        let id = started.id.unwrap();
        for _ in 0..2 {
            stop.call(StopWatchArgs { id }).await.unwrap();
        }
        stop.call(StopWatchArgs { id: id + 100 }).await.unwrap();
        let page = metrics.render();
        assert!(page.contains("scout_price_watches_ended_total{why=\"stopped\"} 1\n"), "{page}");
    }

    #[tokio::test]
    async fn the_model_is_told_what_the_tool_does_in_the_numbers_the_watch_keeps() {
        let (store, _d) = setup();
        let shop = a_shop().await;
        let told = watching(&store, &shop, 1, ReplyTo::telegram(99)).description();
        assert!(told.contains(&format!("after {} days", watch::DAYS)), "got: {told}");
        // What keeps the amounts Rust's: the sentence is handed over whole,
        // and the model is told to pass no price and to change none.
        assert!(told.contains("Never pass or state a current price of your own"), "got: {told}");
        assert!(told.contains("every amount and date exactly as written"), "got: {told}");
    }

    #[tokio::test]
    async fn a_shop_that_refused_a_page_in_this_request_is_not_asked_again_for_a_watch() {
        // `fetch_page` was told no a moment ago and told the model so.
        // The watch tool reads a page too, and a second tool must not be
        // a way to send the shop another request, and a Chrome after it.
        let (store, _d) = setup();
        let shop = a_shop().await;
        let shops: Arc<ShopPages> = Arc::default();
        shops.note_closed("127.0.0.1");
        let tool = watching_in(&store, &shop, 1, ReplyTo::telegram(99), shops);

        let answer = tool.call(under(&shop, "widget", 300.0)).await.unwrap();

        assert!(!answer.watching);
        assert!(answer.say.starts_with("I cannot watch this page: 127.0.0.1 refused Scout's request"), "got: {}", answer.say);
        assert_eq!(asked(&shop).await, 0, "the shop was asked again");
    }

    #[tokio::test]
    async fn a_refusal_the_watch_tool_meets_is_the_requests_to_remember() {
        let (store, _d) = setup();
        let shop = a_shop_that_refuses().await;
        let shops: Arc<ShopPages> = Arc::default();
        let tool = watching_in(&store, &shop, 1, ReplyTo::telegram(99), shops.clone());

        let first = tool.call(under(&shop, "widget", 300.0)).await.unwrap();
        assert!(!first.watching && first.say.contains("refused Scout's request"), "got: {}", first.say);
        assert!(shops.closed("127.0.0.1"), "the delivery desk of this request is not told");

        // Nor is the shop asked for the next page of it.
        let second = tool.call(under(&shop, "another", 300.0)).await.unwrap();
        assert!(!second.watching);
        assert_eq!(asked(&shop).await, 1);
    }

    #[tokio::test]
    async fn the_watch_tool_reads_three_pages_a_request_and_says_so_at_the_fourth() {
        // Without a bound of its own it was a way round `fetch_page`'s:
        // a price and a name for any page, as often as the model asked,
        // each with a render behind it.
        let (store, _d) = setup();
        let shop = a_shop().await;
        let tool = watching(&store, &shop, 1, ReplyTo::telegram(99));
        for page in ["one", "two", "three"] {
            // Over the price, so nothing starts: reading is what is counted.
            let answer = tool.call(under(&shop, page, 1_000_000.0)).await.unwrap();
            assert!(answer.say.starts_with("Widget is already €329.00"), "{page}: {}", answer.say);
        }
        let err = tool.call(under(&shop, "four", 300.0)).await.unwrap_err().to_string();
        assert!(err.contains("has read 3 pages in this request"), "got: {err}");
        assert!(err.contains("do not call it again"), "the model is told to stop: {err}");
        assert_eq!(asked(&shop).await, 3, "the fourth page was not asked for");
        assert!(store.live_watches(1).unwrap().is_empty());

        // The next request has its own three.
        let next = watching(&store, &shop, 1, ReplyTo::telegram(99));
        assert!(next.call(under(&shop, "four", 300.0)).await.unwrap().watching);
    }

    #[tokio::test]
    async fn a_call_that_reads_no_page_spends_none_of_the_three() {
        let (store, _d) = setup();
        let shop = a_shop().await;
        let tool = watching(&store, &shop, 1, ReplyTo::telegram(99));
        assert!(tool.call(under(&shop, "widget", 300.0)).await.unwrap().watching);
        // Asked again for the page it is watching: answered from the table.
        for _ in 0..5 {
            let again = tool.call(under(&shop, "widget", 280.0)).await.unwrap();
            assert!(again.say.starts_with("I am already watching"), "got: {}", again.say);
        }
        assert_eq!(asked(&shop).await, 1);
        assert!(tool.call(under(&shop, "second", 300.0)).await.unwrap().watching);
        assert!(tool.call(under(&shop, "third", 300.0)).await.unwrap().watching);
        assert!(tool.call(under(&shop, "fourth", 300.0)).await.is_err());
    }

    #[test]
    fn the_request_hands_the_watch_tool_its_own_memo_of_the_shops() {
        // The memo is one a request, shared with `fetch_page` and the
        // delivery desk. A source assertion, as a built agent does not
        // say what its tools hold.
        let src = include_str!("../agent.rs");
        let src = &src[..src.find("#[cfg(test)]").expect("the tests must come last")];
        let build = &src[src.find("pub fn build_agent").expect("the agent build must exist")..];
        assert!(build.contains("let shops_for_watches = shops.clone();"), "the watch tool needs the request's memo");
        assert!(build.contains("shops_for_watches.clone(),"), "and has to be handed it");
    }

    #[test]
    fn the_tools_are_named_as_the_design_names_them() {
        assert_eq!(WatchPriceTool::NAME, "watch_price");
        assert_eq!(ListWatchesTool::NAME, "list_watches");
        assert_eq!(StopWatchTool::NAME, "stop_watch");
    }
}
