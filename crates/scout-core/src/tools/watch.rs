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

pub struct WatchPriceTool {
    pub store: Store,
    pub account_id: i64,
    /// Where the watch's one message will go: the chat this run is in.
    /// Carried, as a reminder's is, because in a group that is the group.
    pub reply_to: ReplyTo,
    /// What `fetch_page` reads with, so a watch reads a page the same way.
    pub web: crate::net_guard::OpenWeb,
    pub renderer: Option<super::browser::Renderer>,
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
             watch was started and `say` is why.",
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
        let reader = PageReader::for_a_request(self.web.clone(), self.renderer.clone());
        let ask = Ask {
            account_id: self.account_id,
            reply_to: &self.reply_to,
            url: &args.url,
            target: args.target,
            currency: &args.currency,
        };
        let answer = watch::start(&self.store, &reader, ask, chrono::Utc::now().naive_utc()).await.map_err(internal)?;
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
        WatchPriceTool {
            store: store.clone(),
            account_id,
            reply_to,
            web: OpenWeb::exempting(*shop.address()),
            renderer: None,
        }
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

    #[test]
    fn the_tools_are_named_as_the_design_names_them() {
        assert_eq!(WatchPriceTool::NAME, "watch_price");
        assert_eq!(ListWatchesTool::NAME, "list_watches");
        assert_eq!(StopWatchTool::NAME, "stop_watch");
    }
}
