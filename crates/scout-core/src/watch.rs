//! A price somebody asked to be told about.
//!
//! "Tell me when it is under 300." A watch is started on a page whose price
//! Rust can read, and that page is read again once a day until one of four
//! things: the price is at or under what was asked; the page has been read
//! three checks running and had no price; nothing at all could be read for
//! seven days; or sixty days are up. Each ends the watch with one message,
//! which says what was read and when and nothing that was not. No model is
//! called to check a watch, and none ever supplies a price: the price read,
//! the comparison and every amount in a message are this file's.
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

/// Daily checks in a row that may read the page and find no price before
/// the watch ends with a message saying so. Checks, not days: they are
/// twenty hours apart or more, and a check that got no reading at all is
/// not one of them, so the three can lie further apart than three days and
/// never closer than forty hours. The message says "checks" for that
/// reason.
pub(crate) const UNREADABLE_CHECKS: i64 = 3;

/// Days running with no reading at all, the shop refusing or the request
/// failing, after which a watch ends with a message that says so. The
/// owner's rule of 2026-11-20: without it a watch its shop walls from the
/// second day is checked for fifty-eight more and read on none, and then
/// says the price never came down. Days by the clock, from the first check
/// that got nothing to the one that ends it.
pub(crate) const UNREACHED_DAYS: i64 = 7;

/// How long one reading of a page may take. Its parts are bounded already,
/// the plain request at 30 s by the client and a render at 45 s
/// (`RENDER_TIMEOUT`), the wait for a free Chrome included. This is the
/// one bound over both, held here so that it does not rest on theirs. It
/// is over their sum, and under the 90 s of silence after which a run
/// takes a tool for stalled and is cut (`STREAM_STALL` in `run.rs`): asked
/// for a watch on a page that never answers, a person is told so.
pub(crate) const READ_BUDGET: Duration = Duration::from_secs(80);

/// How long after a check a watch is due its next: the design's twenty
/// hours. The tick that looks is hourly, so a watch is read every twenty
/// hours or twenty-one: somewhat oftener than daily, at an hour that
/// moves round the clock. A wait of twenty-four would be met by the same
/// hour's tick or missed by a moment and left to the next, and "once a
/// day" would then be less than once a day.
const CHECK_EVERY_HOURS: i64 = 20;

/// The least time between two reads of one host in a pass, the design's
/// figure. Several people can watch one shop, and a daily check has no
/// reason to arrive as a burst.
const SPACING: Duration = Duration::from_secs(5);

/// How long a pass may go on taking up watches. It runs inside the hourly
/// maintenance tick, ahead of the nightly backup, and reads one page at a
/// time, so it is cut well short of the hour; what it leaves is still due
/// and is taken up by the next tick, longest unchecked first.
const PASS_BUDGET: Duration = Duration::from_secs(30 * 60);

/// How long a watch that has ended is kept: the design's ninety days.
pub(crate) const KEEP_ENDED_DAYS: i64 = 90;

/// How a check went, as `/metrics` and the log count it: a price was
/// `read`; the page was `unreadable`, read and without a price, or gone;
/// the shop `refused` its reader; or the request `failed`. The design had
/// the first three. The fourth was `unreadable` until an outage of forty
/// hours was seen to be able to end every watch it touched, each one
/// saying its page had no price.
pub(crate) const READ: &str = "read";
pub(crate) const UNREADABLE: &str = "unreadable";
pub(crate) const REFUSED: &str = "refused";
pub(crate) const FAILED: &str = "failed";

/// Why a watch ended, as the table, the outbox key and `/metrics` write
/// it. `unreadable` is one more, in the word above: the page was read and
/// had no price. `unreached` is the other thing: for a week nothing could
/// be read at all. A stop is its owner's doing and the store's to write;
/// the word is here for the count.
pub(crate) const HIT: &str = "hit";
pub(crate) const UNREACHED: &str = "unreached";
pub(crate) const EXPIRED: &str = "expired";
pub(crate) const STOPPED: &str = "stopped";

/// The longest address a watch is started on. An address is kept, read
/// daily and sent back in a message; the longest a browser will take is
/// about this, and one longer is not somebody's product page.
const MAX_URL_CHARS: usize = 2000;

/// The longest name of a product a watch keeps. It is said back in every
/// message about the watch, and a shop's name for a thing can be a
/// paragraph.
const MAX_TITLE_CHARS: usize = 120;

/// What reading a watched page came to. There are three ways to come
/// back without a price and they are not alike, so they are kept apart
/// from here to the message that ends a watch.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Reading {
    Price(Priced),
    /// The page was read and its data prices nothing, or the page is gone
    /// (a 404, a 410). This is the page's own doing, and what the three
    /// checks count.
    Unreadable(String),
    /// The site refused its reader, with a block notice or a bot check.
    /// Nothing was learnt of the page, which may be priced as it ever was.
    Refused,
    /// The request came to nothing and nobody refused it: it failed on the
    /// way, was not answered in time, was answered with a status that is
    /// no page (a 403, a 429, a 5xx), or brought a shell that no browser
    /// filled. Nothing was learnt of the page here either, and the cause
    /// may be on Scout's side as well as the shop's.
    Failed(String),
}

impl Reading {
    /// Why no price was read, for the sentence "this page cannot be
    /// watched: …" and for the log; `None` for a price. The words name no
    /// address, no shop and no product, so that the log line they go into
    /// says which watch and why and not what somebody is watching.
    pub(crate) fn why(&self) -> Option<&str> {
        match self {
            Reading::Price(_) => None,
            Reading::Unreadable(why) | Reading::Failed(why) => Some(why),
            Reading::Refused => Some("the site refused its reader"),
        }
    }
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
    /// Whether the block says the product cannot be bought now. Not
    /// watched and not waited out: a hit says it beside the price.
    pub out_of_stock: bool,
}

/// Something that reads a watched page. The daily pass is written against
/// this and not against the network, so that its spacing and its endings
/// can be tested on a clock the test holds. The read can be sent to
/// another thread, since the pass runs in a task of its own.
pub(crate) trait Reader {
    fn read(&self, url: &str) -> impl std::future::Future<Output = Reading> + Send;
}

/// The page read the way `fetch_page` reads one: the plain request, then
/// the render where one is worth it, both behind the guard. There is no
/// second fetcher, so a watch reads what the model was shown when it
/// opened the same address.
pub(crate) struct PageReader {
    tool: FetchPageTool,
}

impl PageReader {
    /// For the `watch_price` calls of one request: a reader apart from
    /// `fetch_page`'s, so the price a watch starts from is read then and
    /// not remembered from a page opened earlier, and noting in the
    /// request's memo, as that one does, a shop that tells it no.
    pub(crate) fn for_a_request(
        web: crate::net_guard::OpenWeb,
        renderer: Option<crate::tools::browser::Renderer>,
        shops: std::sync::Arc<crate::tools::delivery::ShopPages>,
    ) -> Self {
        Self { tool: FetchPageTool::new(web, renderer).noting(shops) }
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
            Ok(page) => match priced(page.product) {
                // A page that is handed back as a shell was not read: a
                // browser was wanted for it and none filled it. A price in
                // its data all the same is a price.
                Reading::Unreadable(_) if page.unrendered => {
                    Reading::Failed("it came back as a shell that no browser filled".to_string())
                }
                reading => reading,
            },
            Err(FetchError::Refused(_)) => Reading::Refused,
            Err(FetchError::Status(status)) if matches!(status.as_u16(), 404 | 410) => {
                Reading::Unreadable(format!("it is gone (HTTP {})", status.as_u16()))
            }
            Err(FetchError::Status(status)) => Reading::Failed(format!("it answered HTTP {}", status.as_u16())),
            // Not the error's own words: reqwest's name the address asked.
            Err(FetchError::Http(_)) => Reading::Failed("the request for it failed".to_string()),
            // What the tool would not open: an address that is no web
            // address, or not a public one. Its words name no address.
            Err(FetchError::Invalid(why)) => Reading::Failed(why),
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
        out_of_stock: product.availability == Some("out of stock"),
    })
}

/// A product's name as a watch keeps it: one line, no longer than a name,
/// and nothing in it that Telegram would make more than text of.
///
/// The name is the page's own words and goes into a message Scout sends
/// unasked. Telegram turns an address into a link, `/word` into a command
/// to tap and `@word` into a mention, so a page, or whoever wrote one,
/// would be choosing what Scout's message links to. A word that is an
/// address by its scheme or its `www.` is left out. Elsewhere the mark
/// that makes the link is taken and the word kept: a dot between a letter
/// or digit and a letter, which is how a host is written and how "2.0"
/// and "1.5 kg" are not; the slash a word opens with; an `@` anywhere.
fn tidy_title(name: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    for word in name.split_whitespace() {
        let lower = word.to_lowercase();
        if lower.contains("://") || lower.starts_with("www.") {
            continue;
        }
        let letters: Vec<char> = word.chars().collect();
        let plain: String = letters
            .iter()
            .enumerate()
            .map(|(at, c)| {
                let before = at.checked_sub(1).and_then(|at| letters.get(at));
                let after = letters.get(at + 1);
                match c {
                    '@' => ' ',
                    // A command is a slash with a word after it. One that
                    // stands alone, between two sizes, is punctuation.
                    '/' if at == 0 && after.is_some_and(|c| c.is_alphanumeric() || *c == '_') => ' ',
                    '.' if before.is_some_and(|c| c.is_alphanumeric()) && after.is_some_and(|c| c.is_alphabetic()) => ' ',
                    other => *other,
                }
            })
            .collect();
        words.extend(plain.split_whitespace().map(str::to_string));
    }
    words.join(" ").chars().take(MAX_TITLE_CHARS).collect::<String>().trim_end().to_string()
}

/// Whether two names are one product's, as far as a name can say: the
/// same letters whatever their case, since both were kept as `tidy_title`
/// keeps one, spacing settled and cut at the same length. A name that is
/// missing on either side is not a different name: there is nothing to set
/// against the other, and nothing is claimed of it.
fn same_name(kept: &str, read_now: Option<&str>) -> bool {
    match read_now {
        Some(now) if !kept.is_empty() && !now.is_empty() => kept.to_lowercase() == now.to_lowercase(),
        _ => true,
    }
}

/// One reading, given up on after `budget`. A page that never answers was
/// not read: a request that failed, not a page without a price.
pub(crate) async fn read_within<R: Reader>(reader: &R, url: &str, budget: Duration) -> Reading {
    match tokio::time::timeout(budget, reader.read(url)).await {
        Ok(reading) => reading,
        Err(_) => Reading::Failed(format!("it did not answer in time ({} s)", budget.as_secs())),
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

/// Where a watch asked for in this run would send its one message, or
/// `None` when there is nowhere it may. Worked out once a run, before its
/// agent is built: a run with nowhere is not offered the watch tools.
///
/// A watch message names a product, a target and a price, and the rule is
/// that a group hears of one only when it was asked for in that group.
/// The run's own address does not say that. From Telegram it is the chat
/// the message came in. From the browser it is wherever the account last
/// spoke on Telegram, which `deliveries` overwrites on every message and
/// which can be a group the request has nothing to do with.
///
/// So an address that is a group's, a negative chat number, is kept only
/// when the run's thread is that group's own (`telegram:<chat>`, the scope
/// the bot gives a group's conversation). Otherwise the watch is the
/// person's: their private chat, whose number is their Telegram id. With
/// no Telegram account, or two and nothing to say which, there is no
/// private chat to be had and no watch is offered. What this cannot know
/// is whether the person has ever started the bot in that private chat;
/// one who has not is not reached, and the outbox counts the message as
/// given up.
///
/// A reminder follows the address as its run has it, the older rule, and
/// is not changed here.
pub(crate) async fn deliver_to(store: &Store, run: &scout_api::RunContext) -> Option<ReplyTo> {
    let reply_to = run.reply_to.clone()?;
    if reply_to.channel != crate::mirror::TELEGRAM {
        return Some(reply_to);
    }
    let chat = reply_to.address.parse::<i64>().ok()?;
    if chat > 0 {
        return Some(reply_to);
    }
    let (theirs, account_id, conversation_id) = (store.clone(), run.account_id, run.conversation_id);
    let looked = blocking(move || Ok((theirs.conversation_scope(conversation_id)?, theirs.telegram_ids(account_id)?))).await;
    let (scope, own) = match looked {
        Ok(found) => found,
        Err(e) => {
            // Not fatal: the run is worth doing without the watch tools.
            tracing::warn!(error = %e, "could not tell where a price watch would be delivered; not offering one");
            return None;
        }
    };
    if scope.as_deref() == Some(format!("telegram:{chat}").as_str()) {
        return Some(reply_to);
    }
    match own.as_slice() {
        [own] => Some(ReplyTo::telegram(*own)),
        _ => None,
    }
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
    /// How the request went, in a word for `/metrics`: `started`, or what
    /// stopped it. A refusal is an answer, and an answer is `ok` to the
    /// tool-call counter, so without this nothing would show how many
    /// requests for a watch became one and what became of the rest.
    pub outcome: &'static str,
}

impl Answer {
    fn not_started(outcome: &'static str, say: String) -> Self {
        Self { started: None, say, outcome }
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
    // A cent is the least a shop prices in, and the comparison is made in
    // cents: under one there is nothing for a price to come down to.
    if !ask.target.is_finite() || ask.target < 0.01 {
        anyhow::bail!("target must be an amount of one cent or more, as the user named it");
    }
    let currency = ask.currency.trim().to_uppercase();
    if currency.is_empty() {
        anyhow::bail!("currency must be the ISO code of the target's currency, for example EUR");
    }
    let url = ask.url.trim().to_string();
    if url.chars().count() > MAX_URL_CHARS {
        return Ok(Answer::not_started("address_too_long", format!(
            "I cannot watch this page: its address is over {MAX_URL_CHARS} characters long. \
             A watch that cannot be checked is not started."
        )));
    }
    let shop = shop_of(&url);

    // Before the page is asked for anything. The store decides both again
    // under its lock when the watch is written; this is what spares a shop
    // a request for a watch that could not start.
    let (theirs, account_id) = (store.clone(), ask.account_id);
    let live = blocking(move || theirs.live_watches(account_id)).await?;
    if let Some(there) = live.iter().find(|watch| watch.url == url) {
        return Ok(Answer::not_started("watching_already", say_already(there)));
    }
    if live.len() as i64 >= MAX_LIVE {
        return Ok(Answer::not_started("at_cap", say_full()));
    }

    let read = match read_within(reader, &url, READ_BUDGET).await {
        Reading::Price(read) => read,
        reading @ (Reading::Unreadable(_) | Reading::Failed(_)) => {
            let outcome = if matches!(reading, Reading::Failed(_)) { FAILED } else { "no_price" };
            let why = reading.why().unwrap_or_default();
            return Ok(Answer::not_started(outcome, format!(
                "I cannot watch this page: {why}. A watch that cannot be checked is not started."
            )));
        }
        Reading::Refused => {
            return Ok(Answer::not_started(REFUSED, format!(
                "I cannot watch this page: {shop} refused Scout's request for it, so there is no price I could \
                 check each day. A watch that cannot be checked is not started."
            )))
        }
    };
    if read.currency != currency {
        return Ok(Answer::not_started("other_currency", format!(
            "I cannot watch this for an amount in {currency}: the page prices it in {theirs} ({price} now), and I do \
             not compare across currencies. Ask again with the amount in {theirs}.",
            theirs = read.currency,
            price = money(read.amount, &read.currency),
        )));
    }
    let title = read.title.clone().unwrap_or_default();
    if at_or_under(read.amount, ask.target) {
        return Ok(Answer::not_started("already_under", format!(
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
        WatchStarted::Started(watch) => {
            Answer { started: Some(watch.id), say: say_watching(&watch, read.struck_through), outcome: "started" }
        }
        WatchStarted::Already(there) => Answer::not_started("watching_already", say_already(&there)),
        WatchStarted::Full => Answer::not_started("at_cap", say_full()),
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
    /// The last price that was read, and the day it was read on, which is
    /// not always the day of the last check: see `note`.
    pub last_price: String,
    pub last_price_read: String,
    pub lowest_price: String,
    pub price_when_started: String,
    pub started: String,
    pub last_checked: String,
    pub checked_until: String,
    /// Said when the last check did not read a price, and why not: the
    /// row would otherwise read as a price seen yesterday.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

pub(crate) fn listed(watch: &PriceWatch) -> Listed {
    let amount = |amount: f64| money(amount, &watch.currency);
    let note = match watch.unreached_since {
        Some(since) => Some(format!(
            "no check has been able to read the page since {}; the price shown was read on {}",
            day(since),
            day(watch.last_read_at)
        )),
        None if watch.unreadable_streak > 0 => Some(format!(
            "the page gave no price at the last {} daily checks that read it; after {UNREADABLE_CHECKS} in a row the watch ends",
            watch.unreadable_streak
        )),
        None => None,
    };
    Listed {
        id: watch.id,
        title: named(&watch.title, false).to_string(),
        shop: shop_of(&watch.url),
        url: watch.url.clone(),
        target: amount(watch.target),
        last_price: amount(watch.last_price),
        last_price_read: day(watch.last_read_at),
        lowest_price: amount(watch.lowest_price),
        price_when_started: amount(watch.first_price),
        started: day(watch.created_at),
        last_checked: day(watch.last_checked_at),
        checked_until: day(watch.expires_at),
        note,
    }
}

pub(crate) fn say_stopped(watch: &PriceWatch) -> String {
    format!("Stopped watching {} at {}.", named(&watch.title, false), shop_of(&watch.url))
}

/// What one check of a watch comes to: what to write, and where it ends
/// the watch, why, with the one message that says so.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Verdict {
    /// `READ`, `UNREADABLE`, `REFUSED` or `FAILED`.
    pub outcome: &'static str,
    pub last_price: f64,
    pub lowest_price: f64,
    pub last_read_at: chrono::NaiveDateTime,
    pub unreadable_streak: i64,
    pub unreached_since: Option<chrono::NaiveDateTime>,
    pub ended: Option<(&'static str, String)>,
}

/// Decides a check from the watch as it stood and what was read. Nothing
/// is read or written here, and the day is handed in.
///
/// Three things can come of a check, and they are kept apart to the end:
/// - a price. It is kept, with when it was read, and at or under the
///   target it is a hit;
/// - the page, read, with no price. That adds to the checks running, and
///   the third ends the watch as `unreadable`. A price in between starts
///   the count again;
/// - no reading at all: the shop refused its reader, or the request
///   failed. Nothing was learnt of the page, so it neither adds to that
///   count nor wipes it, and it changes no price. What it does is start a clock, if none is running,
///   which any reading of the page stops; a week on that clock ends the
///   watch as `unreached`, saying since when nothing could be read. A shop
///   that walls the reader for three days and serves it on the fourth has
///   not lost its price, and one that walls it for good must not leave the
///   watch looking checked.
///
/// Then, for a watch still live and past its sixty days, the end of those
/// days: so on its last day a hit is still a hit.
pub(crate) fn decide(watch: &PriceWatch, reading: &Reading, now: chrono::NaiveDateTime) -> Verdict {
    let mut verdict = Verdict {
        outcome: REFUSED,
        last_price: watch.last_price,
        lowest_price: watch.lowest_price,
        last_read_at: watch.last_read_at,
        unreadable_streak: watch.unreadable_streak,
        unreached_since: watch.unreached_since,
        ended: None,
    };
    match reading {
        Reading::Price(read) if read.currency == watch.currency => {
            verdict.outcome = READ;
            verdict.last_price = read.amount;
            verdict.lowest_price = watch.lowest_price.min(read.amount);
            verdict.last_read_at = now;
            verdict.unreadable_streak = 0;
            verdict.unreached_since = None;
            if at_or_under(read.amount, watch.target) {
                verdict.ended = Some((HIT, say_hit(watch, read)));
            }
        }
        // A price in another currency than the watch's is no price for it:
        // nothing is compared across currencies, here as at the start.
        Reading::Price(_) | Reading::Unreadable(_) => {
            verdict.outcome = UNREADABLE;
            verdict.unreadable_streak += 1;
            verdict.unreached_since = None;
            if verdict.unreadable_streak >= UNREADABLE_CHECKS {
                verdict.ended = Some((UNREADABLE, say_unreadable(watch)));
            }
        }
        Reading::Refused | Reading::Failed(_) => {
            if matches!(reading, Reading::Failed(_)) {
                verdict.outcome = FAILED;
            }
            let since = watch.unreached_since.unwrap_or(now);
            verdict.unreached_since = Some(since);
            if now - since >= chrono::Duration::days(UNREACHED_DAYS) {
                verdict.ended = Some((UNREACHED, say_unreached(watch, since)));
            }
        }
    }
    if verdict.ended.is_none() && now >= watch.expires_at {
        verdict.ended = Some((EXPIRED, say_expired(watch, verdict.lowest_price, verdict.last_read_at)));
    }
    verdict
}

/// The hit. Beside the amounts it says two things the block may say that
/// the person would otherwise find out at the shop.
///
/// That the page says the product is out of stock: said, and not waited
/// out, by the owner's word of 2026-11-20. The price is what was asked
/// about.
///
/// And that the page now names another thing than the watch was started
/// on. A discontinued product's address can redirect to its accessory, or
/// a page be given to the next model, and the price read there is under
/// the target and not the thing's. Nothing in the data says so but the
/// name, so the name is shown and the person asked to look; see
/// `same_name` for what counts as another. The watch ends all the same:
/// whatever the page has become, it is no longer the watch that was asked
/// for, and one message is what was promised.
fn say_hit(watch: &PriceWatch, read: &Priced) -> String {
    let mut said = format!(
        "{} is {} at {} — {} you asked for. It was {} on {}.",
        named(&watch.title, true),
        money(read.amount, &watch.currency),
        shop_of(&watch.url),
        against(read.amount, watch.target, &watch.currency),
        money(watch.first_price, &watch.currency),
        day(watch.created_at),
    );
    if read.out_of_stock {
        said.push_str(" The page says it is out of stock.");
    }
    if let Some(now) = read.title.as_deref().filter(|now| !same_name(&watch.title, Some(now))) {
        said.push_str(&format!(" The page now calls this \"{now}\": please check it is still what you meant."));
    }
    format!("{said} {}", watch.url)
}

/// The page was read and had no price. Said in checks, which is what was
/// counted: three of them are not always three days.
fn say_unreadable(watch: &PriceWatch) -> String {
    format!(
        "I have stopped watching {} at {}: its page had no price for me at the last {UNREADABLE_CHECKS} daily \
         checks that read it. The last price I read was {}, on {}; you asked to be told at {} or less. {}",
        named(&watch.title, false),
        shop_of(&watch.url),
        money(watch.last_price, &watch.currency),
        day(watch.last_read_at),
        money(watch.target, &watch.currency),
        watch.url,
    )
}

/// Nothing could be read for a week. It does not say the shop refused:
/// the same week can be a shop's wall or requests that failed, and what
/// is true of both is that the page was not read.
fn say_unreached(watch: &PriceWatch, since: chrono::NaiveDateTime) -> String {
    format!(
        "I have stopped watching {} at {}: I have not been able to read its page since {}, {UNREACHED_DAYS} days \
         running. The last price I read was {}, on {}; you asked to be told at {} or less. {}",
        named(&watch.title, false),
        shop_of(&watch.url),
        day(since),
        money(watch.last_price, &watch.currency),
        day(watch.last_read_at),
        money(watch.target, &watch.currency),
        watch.url,
    )
}

/// The sixty days are up. What is said is what was read: the lowest price
/// and when a price was last read. Not that the price stayed above the
/// target, which nobody knows of the days the page was not read.
fn say_expired(watch: &PriceWatch, lowest: f64, last_read_at: chrono::NaiveDateTime) -> String {
    format!(
        "My {} days of watching {} at {} are up, and I did not see it at {} or less. \
         The lowest price I read was {}; the last time I read a price was {}. {}",
        (watch.expires_at - watch.created_at).num_days(),
        named(&watch.title, false),
        shop_of(&watch.url),
        money(watch.target, &watch.currency),
        money(lowest, &watch.currency),
        day(last_read_at),
        watch.url,
    )
}

/// How a pass paces itself. `Pace::DAILY` is the one production runs at;
/// a test hands in another to not wait out the seconds it is not about.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Pace {
    pub spacing: Duration,
    pub read_budget: Duration,
    pub pass_budget: Duration,
}

impl Pace {
    pub(crate) const DAILY: Pace = Pace { spacing: SPACING, read_budget: READ_BUDGET, pass_budget: PASS_BUDGET };
}

/// What a pass did, for the log line and for whoever wakes the channel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Pass {
    /// Watches that were due when the pass began.
    pub due: usize,
    /// Of those, the ones read and written. One stopped by its owner while
    /// its page was read is not among them.
    pub checked: usize,
    /// Watches this pass ended, each with its one message queued.
    pub ended: usize,
    /// Still due when the pass ran out of time: the next tick's.
    pub left: usize,
    /// How long the pass took, its waits between two reads of a host
    /// included. For the log: the first week will say whether thirty
    /// minutes is room enough.
    pub took: Duration,
}

/// Checks every watch that is due at `now`: reads its page, decides, and
/// writes what came of it. One page at a time, so the pass never holds
/// more than one of the two Chromes a person's request may be waiting for.
///
/// The store's lock is held to list the watches and again to write each
/// check, and never while a page is being read: a pass that waited on the
/// network with it would stall every chat. A watch stopped in that gap is
/// left as its owner left it; `record_watch_check` sees to that.
///
/// The pass ends whatever a host does: each read is given up on after
/// `read_budget`, and after `pass_budget` no further watch is taken up.
/// One watch's failure to be written is logged and the pass goes on; it is
/// still due, and the next tick tries it again.
///
/// And whatever a page does. A watch is stamped as checked before its page
/// is asked for, so that a read which never comes back has still cost the
/// watch its turn; and a read that panics, the parser on something a shop
/// served, is caught here and counted as a request that came to nothing.
/// Uncaught it would take the pass with it, and that page would be read
/// first, and panic first, at every pass after.
///
/// Each check is counted on `metrics` by how it went, and each watch the
/// pass ends by why.
pub(crate) async fn check_due<R: Reader>(
    store: &Store,
    metrics: &crate::metrics::Metrics,
    reader: &R,
    now: chrono::NaiveDateTime,
    pace: &Pace,
) -> anyhow::Result<Pass> {
    let theirs = store.clone();
    let due = blocking(move || theirs.due_watches(now, now - chrono::Duration::hours(CHECK_EVERY_HOURS))).await?;
    let began = tokio::time::Instant::now();
    let mut pass = Pass { due: due.len(), ..Pass::default() };
    // When each host was last asked, by the name a shop is called by: with
    // its `www.` or without, it is one shop's server.
    let mut asked: std::collections::HashMap<String, tokio::time::Instant> = std::collections::HashMap::new();
    for (n, watch) in due.iter().enumerate() {
        if began.elapsed() >= pace.pass_budget {
            pass.left = due.len() - n;
            tracing::warn!(left = pass.left, "the price-watch pass ran out of time; the rest wait for the next tick");
            break;
        }
        let host = shop_of(&watch.url);
        if let Some(last) = asked.get(&host) {
            tokio::time::sleep_until(*last + pace.spacing).await;
        }
        let id = watch.id;
        let theirs = store.clone();
        match blocking(move || theirs.mark_watch_checking(id, now)).await {
            Ok(true) => {}
            // Stopped since the pass listed it: the shop is not asked.
            Ok(false) => continue,
            Err(e) => {
                tracing::warn!(watch = id, error = %e, "could not stamp a price watch as checked; left for the next tick");
                continue;
            }
        }
        let read = std::panic::AssertUnwindSafe(read_within(reader, &watch.url, pace.read_budget));
        let reading = match futures::FutureExt::catch_unwind(read).await {
            Ok(reading) => reading,
            Err(_) => {
                tracing::error!(watch = id, "reading a price watch's page panicked; counted as a request that failed");
                Reading::Failed("reading it panicked".to_string())
            }
        };
        asked.insert(host, tokio::time::Instant::now());

        let verdict = decide(watch, &reading, now);
        // Counted as a check whatever becomes of the write below: the
        // request was made, and how shops answer is what this counts.
        metrics.watch_checked(verdict.outcome);
        // And said, where no price was read: which watch, how it went and
        // why. Not the address, the shop or the product, which are one
        // person's business; the watch's number finds them for whoever
        // may look.
        if let Some(why) = reading.why() {
            tracing::info!(watch = id, outcome = verdict.outcome, why, "a price watch's check read no price");
        }
        let (theirs, decided) = (store.clone(), verdict.clone());
        let written = blocking(move || {
            theirs.record_watch_check(&crate::store::WatchCheck {
                id,
                now,
                last_price: decided.last_price,
                lowest_price: decided.lowest_price,
                last_read_at: decided.last_read_at,
                unreadable_streak: decided.unreadable_streak,
                unreached_since: decided.unreached_since,
                ended: decided.ended.as_ref().map(|(why, say)| (*why, say.as_str())),
            })
        })
        .await;
        match written {
            Ok(true) => {
                pass.checked += 1;
                if let Some((why, _)) = &verdict.ended {
                    pass.ended += 1;
                    metrics.watch_ended(why);
                    tracing::info!(watch = id, why, "a price watch ended");
                }
            }
            Ok(false) => tracing::info!(watch = id, "a price watch was stopped while its page was read; nothing written"),
            Err(e) => tracing::warn!(watch = id, error = %e, "could not write a price watch's check; it stays due"),
        }
    }
    pass.took = began.elapsed();
    // How many are running after the pass, for `/metrics`: counted here,
    // once an hour, so the number is as old as the last pass and costs
    // the page nothing to show.
    let theirs = store.clone();
    match blocking(move || theirs.count_live_watches()).await {
        Ok(live) => metrics.watches_live(live),
        Err(e) => tracing::warn!(error = %e, "could not count the live price watches"),
    }
    Ok(pass)
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
        PageReader::for_a_request(OpenWeb::exempting(*server.address()), None, Default::default())
    }

    async fn read(server: &MockServer) -> Reading {
        reader(server).read(&format!("{}/p/widget", server.uri())).await
    }

    #[tokio::test]
    async fn a_price_is_read_from_the_pages_own_data_and_never_from_its_text() {
        let server = serving(200, &widget_at("329.00")).await;
        assert_eq!(
            read(&server).await,
            Reading::Price(Priced { amount: 329.0, currency: "EUR".into(), title: Some("Widget".into()), struck_through: None, out_of_stock: false })
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

    /// A page that is all script: nothing to read until a browser runs it.
    const SHELL: &str = "<html><head><title>Shop</title></head><body><div id=\"root\"></div></body></html>";

    #[tokio::test]
    async fn only_a_page_that_was_read_or_is_gone_counts_as_a_page_with_no_price() {
        // Three kinds of nothing, kept apart from here on. A page read
        // and found without a price, or gone, counts towards ending a
        // watch for having no price. A shop that walls the reader has
        // said nothing about the page. And a request that failed has
        // said nothing either: a bad minute at the shop, or an outage of
        // Scout's own, is not a page that lost its price.
        for status in [404, 410] {
            let server = serving(status, "").await;
            assert_eq!(read(&server).await, Reading::Unreadable(format!("it is gone (HTTP {status})")), "{status}");
        }
        for status in [403, 429, 500, 503] {
            let server = serving(status, "").await;
            assert_eq!(read(&server).await, Reading::Failed(format!("it answered HTTP {status}")), "{status}");
        }
        let challenge = "<html><head><title>Just a moment...</title></head><body></body></html>";
        for wall in [NOTICE, challenge] {
            let server = serving(200, wall).await;
            assert_eq!(read(&server).await, Reading::Refused, "for {wall}");
        }

        // A shell no browser filled is a page not read: no browser here,
        // or one that would not start.
        let server = serving(200, SHELL).await;
        let unfilled = Reading::Failed("it came back as a shell that no browser filled".to_string());
        assert_eq!(read(&server).await, unfilled);
        let stalled = crate::tools::browser::Renderer::new(std::path::PathBuf::from("/definitely/not/chrome"));
        let with_one = PageReader::for_a_request(OpenWeb::exempting(*server.address()), Some(stalled), Default::default());
        assert_eq!(with_one.read(&format!("{}/p/widget", server.uri())).await, unfilled);
        // Where a status that is no page sent the request to a browser
        // that would not start, it is the status that is told.
        let server = serving(503, "").await;
        let stalled = crate::tools::browser::Renderer::new(std::path::PathBuf::from("/definitely/not/chrome"));
        let with_one = PageReader::for_a_request(OpenWeb::exempting(*server.address()), Some(stalled), Default::default());
        assert_eq!(with_one.read(&format!("{}/p/widget", server.uri())).await, Reading::Failed("it answered HTTP 503".to_string()));

        // A request that never arrived: a port nobody listens on any more.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let nobody = PageReader::for_a_request(OpenWeb::exempting(address), None, Default::default());
        assert_eq!(
            nobody.read(&format!("http://{address}/p/widget")).await,
            Reading::Failed("the request for it failed".to_string())
        );
    }

    #[tokio::test]
    async fn what_is_logged_of_a_check_names_no_address_no_shop_and_no_product() {
        // Every check that reads no price is logged with why, and which
        // page a person watches is theirs: the line carries the watch's
        // number and a reason, never where or what.
        let server = MockServer::start().await;
        for (page, status, body) in [
            ("secret-widget-gone", 404, String::new()),
            ("secret-widget-busy", 503, String::new()),
            ("secret-widget-walled", 200, NOTICE.to_string()),
            ("secret-widget-shell", 200, SHELL.to_string()),
            ("secret-widget-unpriced", 200, format!("<html><body><h1>Secret Widget</h1>{}</body></html>", "<p>A widget for every purpose.</p>".repeat(100))),
        ] {
            Mock::given(method("GET")).and(path(format!("/p/{page}")))
                .respond_with(ResponseTemplate::new(status).set_body_string(body)).mount(&server).await;
        }
        let host = server.address().to_string();
        let mut seen = Vec::new();
        for page in ["gone", "busy", "walled", "shell", "unpriced"] {
            // A reader a page, as a watch on another shop would be: the
            // wall must not close the host for the pages after it.
            let reading = reader(&server).read(&format!("{}/p/secret-widget-{page}", server.uri())).await;
            let why = reading.why().expect("not a price").to_string();
            for private in [host.as_str(), "127.0.0.1", "secret", "widget", "Widget", "/p/", "http"] {
                assert!(!why.contains(private), "{page}: {why:?} gives away {private:?}");
            }
            seen.push(why);
        }
        assert_eq!(
            seen,
            [
                "it is gone (HTTP 404)",
                "it answered HTTP 503",
                "the site refused its reader",
                "it came back as a shell that no browser filled",
                "its data states no product",
            ]
        );
        assert_eq!(read_within(&Fixed::new(price(1.0)), "https://shop.example/p", READ_BUDGET).await.why(), None);
        // And the line itself is built from those and the watch's number.
        let src = include_str!("watch.rs");
        let src = &src[..src.find("#[cfg(test)]").expect("the tests come last")];
        assert!(src.contains(r#"tracing::info!(watch = id, outcome = verdict.outcome, why, "a price watch's check read no price")"#));
        for line in src.lines().filter(|l| l.contains("tracing::")) {
            for private in ["url", "title", "shop", "host", "address"] {
                assert!(!line.contains(private), "a log line of the pass names the {private}: {line}");
            }
        }
    }

    #[tokio::test]
    async fn an_address_that_is_not_public_is_refused_by_the_guard_and_never_opened() {
        // The address is the model's word. A watch must not be a way to
        // have Scout ask the pod's own listeners something once a day.
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_string(widget_at("1.00"))).expect(0).mount(&server).await;
        let guarded = PageReader::for_a_request(OpenWeb::new(std::time::Duration::from_secs(5)).unwrap(), None, Default::default());
        for url in [format!("{}/metrics", server.uri()), "http://10.43.0.1/".to_string(), "ftp://shop.example/p".to_string()] {
            match guarded.read(&url).await {
                Reading::Failed(why) => assert!(why.contains("not on the public internet") || why.contains("scheme"), "{url}: {why}"),
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
            Self::new(Reading::Price(Priced { amount, currency: "EUR".into(), title: Some("Widget".into()), struck_through: None, out_of_stock: false }))
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

    /// Where a watch asked for in this run would be delivered: by an
    /// account with these Telegram ids, in a thread of this scope, its
    /// channel having handed the run this address.
    async fn delivered_to(telegram: &[i64], scope: &str, reply_to: Option<ReplyTo>) -> Option<ReplyTo> {
        let (store, _dir) = a_store();
        let account_id = store.account_for_identity("email", "a@example.com").unwrap();
        for id in telegram {
            store.link_identity(account_id, "telegram", &id.to_string()).unwrap();
        }
        let conversation_id = store.start_conversation(account_id, scope).unwrap();
        deliver_to(&store, &scout_api::RunContext { account_id, conversation_id, reply_to, title_source: None }).await
    }

    #[tokio::test]
    async fn a_watch_goes_to_a_group_only_when_it_was_asked_for_in_that_group() {
        let own = ReplyTo::telegram(4242);
        let group = ReplyTo::telegram(-100_123);

        // On Telegram the run's address is the chat the message came in.
        assert_eq!(delivered_to(&[4242], "direct", Some(own.clone())).await, Some(own.clone()));
        assert_eq!(delivered_to(&[4242], "telegram:-100123", Some(group.clone())).await, Some(group.clone()));

        // From the browser it is wherever the account last spoke, which
        // can be a group the request has nothing to do with. Then the
        // watch is the person's own: their private chat, whose number is
        // their Telegram id.
        assert_eq!(delivered_to(&[4242], "direct", Some(group.clone())).await, Some(own.clone()));
        // Another group's thread is not this group either.
        assert_eq!(delivered_to(&[4242], "telegram:-100999", Some(group.clone())).await, Some(own));

        // And where no private chat is to be had there is nowhere: no
        // Telegram account at all, or two and nothing to say which.
        assert_eq!(delivered_to(&[], "direct", Some(group.clone())).await, None);
        assert_eq!(delivered_to(&[4242, 4343], "direct", Some(group)).await, None);
        assert_eq!(delivered_to(&[4242], "direct", None).await, None);
        // An address that is not a chat's number is not guessed at.
        let odd = ReplyTo { channel: "telegram".into(), address: "somewhere".into() };
        assert_eq!(delivered_to(&[4242], "direct", Some(odd)).await, None);
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
            out_of_stock: false,
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
            (Reading::Unreadable("it is gone (HTTP 404)".into()), "I cannot watch this page: it is gone (HTTP 404)."),
            (Reading::Failed("it answered HTTP 503".into()), "I cannot watch this page: it answered HTTP 503."),
            (
                Reading::Refused,
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
    async fn an_address_too_long_to_be_one_and_a_target_under_a_cent_start_nothing() {
        let (store, _dir) = a_store();
        let chat = ReplyTo::telegram(4242);
        let read = Fixed::priced(329.0);
        let long = format!("https://shop.example/p/{}", "w".repeat(2000));
        let answer = start(&store, &read, ask(&chat, &long, 300.0), clock()).await.unwrap();
        assert_eq!(answer.started, None);
        assert_eq!(answer.say, "I cannot watch this page: its address is over 2000 characters long. A watch that cannot be checked is not started.");
        let just = format!("https://shop.example/p/{}", "w".repeat(2000 - "https://shop.example/p/".len()));
        assert_eq!(just.chars().count(), 2000);
        assert!(start(&store, &read, ask(&chat, &just, 300.0), clock()).await.unwrap().started.is_some());

        // Under a cent there is no price to come down to.
        for target in [0.009, 0.004, 0.0001] {
            let err = start(&store, &read, ask(&chat, WIDGET, target), clock()).await.unwrap_err();
            assert!(err.to_string().contains("one cent"), "{target}: {err}");
        }
        let cent = Fixed::priced(0.05);
        assert!(start(&store, &cent, ask(&chat, "https://shop.example/p/penny", 0.01), clock()).await.unwrap().started.is_some());
        assert_eq!(read.asked(), 1, "only the watch that started read its page");
    }

    #[tokio::test]
    async fn every_answer_says_how_the_request_went_in_a_word_for_the_count() {
        // A refusal is an answer, and to the tool-call counter an answer
        // is `ok`. What became of requests for a watch would otherwise
        // show nowhere: how many started, and what stopped the rest.
        let chat = ReplyTo::telegram(4242);
        async fn asked(reading: Reading, target: f64, currency: &str) -> &'static str {
            let (store, _dir) = a_store();
            let chat = ReplyTo::telegram(4242);
            let ask = Ask { currency, ..ask(&chat, WIDGET, target) };
            start(&store, &Fixed::new(reading), ask, clock()).await.unwrap().outcome
        }
        assert_eq!(asked(price(329.0), 300.0, "EUR").await, "started");
        assert_eq!(asked(price(289.0), 300.0, "EUR").await, "already_under");
        assert_eq!(asked(price(329.0), 300.0, "USD").await, "other_currency");
        assert_eq!(asked(no_price(), 300.0, "EUR").await, "no_price");
        assert_eq!(asked(refused(), 300.0, "EUR").await, "refused");
        assert_eq!(asked(failed(), 300.0, "EUR").await, "failed");

        let (store, _dir) = a_store();
        let read = Fixed::priced(329.0);
        assert_eq!(start(&store, &read, ask(&chat, WIDGET, 300.0), clock()).await.unwrap().outcome, "started");
        assert_eq!(start(&store, &read, ask(&chat, WIDGET, 280.0), clock()).await.unwrap().outcome, "watching_already");
        let long = format!("https://shop.example/p/{}", "w".repeat(2000));
        assert_eq!(start(&store, &read, ask(&chat, &long, 300.0), clock()).await.unwrap().outcome, "address_too_long");
        for n in 1..10 {
            start(&store, &read, ask(&chat, &format!("https://shop.example/p/{n}"), 300.0), clock()).await.unwrap();
        }
        assert_eq!(start(&store, &read, ask(&chat, "https://shop.example/p/10", 300.0), clock()).await.unwrap().outcome, "at_cap");
    }

    #[tokio::test]
    async fn a_page_that_names_no_product_is_watched_as_this_product() {
        let (store, _dir) = a_store();
        let chat = ReplyTo::telegram(4242);
        let nameless = Fixed::new(Reading::Price(Priced { amount: 329.0, currency: "EUR".into(), title: None, struck_through: None, out_of_stock: false }));
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
        assert!(answer.say.starts_with("I cannot watch this page: it did not answer in time (80 s)."), "got: {}", answer.say);
    }

    #[tokio::test]
    async fn a_listed_watch_has_its_amounts_and_days_written_out() {
        let (store, _dir) = a_store();
        let now = clock();
        let chat = ReplyTo::telegram(4242);
        let id = start(&store, &Fixed::priced(329.0), ask(&chat, WIDGET, 300.0), now).await.unwrap().started.unwrap();
        let later = now + chrono::Duration::days(2);
        let check = crate::store::WatchCheck {
            id,
            now: later,
            last_price: 310.0,
            lowest_price: 310.0,
            last_read_at: later,
            unreadable_streak: 0,
            unreached_since: None,
            ended: None,
        };
        assert!(store.record_watch_check(&check).unwrap());

        let row = listed(&store.live_watches(1).unwrap()[0]);
        assert_eq!((row.id, row.title.as_str(), row.shop.as_str(), row.url.as_str()), (id, "Widget", "shop.example", WIDGET));
        assert_eq!((row.target.as_str(), row.last_price.as_str(), row.lowest_price.as_str()), ("€300.00", "€310.00", "€310.00"));
        assert_eq!(row.price_when_started, "€329.00");
        assert_eq!(row.started, now.format("%-d %B").to_string());
        assert_eq!(row.last_price_read, later.format("%-d %B").to_string());
        assert_eq!(row.last_checked, later.format("%-d %B").to_string());
        assert_eq!(row.checked_until, (now + chrono::Duration::days(60)).format("%-d %B").to_string());
        assert_eq!(row.note, None);

        // A check that read the page and found no price leaves the price
        // as it was, and the row says so.
        let next = later + chrono::Duration::days(1);
        assert!(store.record_watch_check(&crate::store::WatchCheck { now: next, unreadable_streak: 2, ..check.clone() }).unwrap());
        let row = listed(&store.live_watches(1).unwrap()[0]);
        assert_eq!(row.last_price, "€310.00");
        assert_eq!(
            row.note.as_deref(),
            Some("the page gave no price at the last 2 daily checks that read it; after 3 in a row the watch ends")
        );
    }

    #[tokio::test]
    async fn a_watch_nobody_has_read_for_weeks_is_not_listed_as_read_yesterday() {
        // The review's case: started at 329.00, and the shop serves a bot
        // check from the second day on. Thirty days in, the watch was
        // checked yesterday and read a month ago, and the row says both.
        let (store, _dir) = a_store();
        let now = clock();
        let began = now - chrono::Duration::days(30);
        let w = watching_since(&store, WIDGET, began).await;
        let walled_since = began + chrono::Duration::days(1);
        let yesterday = now - chrono::Duration::days(1);
        let check = crate::store::WatchCheck {
            id: w.id,
            now: yesterday,
            last_price: 329.0,
            lowest_price: 329.0,
            last_read_at: began,
            unreadable_streak: 0,
            unreached_since: Some(walled_since),
            ended: None,
        };
        assert!(store.record_watch_check(&check).unwrap());

        let row = listed(&store.live_watches(1).unwrap()[0]);
        assert_eq!(row.last_price, "€329.00");
        assert_eq!(row.last_price_read, began.format("%-d %B").to_string());
        assert_eq!(row.last_checked, yesterday.format("%-d %B").to_string());
        assert_eq!(
            row.note,
            Some(format!(
                "no check has been able to read the page since {}; the price shown was read on {}",
                walled_since.format("%-d %B"),
                began.format("%-d %B")
            ))
        );
    }

    /// A reader that answers each address by a rule of the test's, and
    /// notes which address it was asked for and when.
    struct Told<F> {
        answer: F,
        asked: std::sync::Mutex<Vec<(String, tokio::time::Instant)>>,
    }

    impl<F: Fn(&str) -> Reading + Sync> Told<F> {
        fn by(answer: F) -> Self {
            Self { answer, asked: std::sync::Mutex::new(Vec::new()) }
        }

        fn asked(&self) -> Vec<(String, tokio::time::Instant)> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl<F: Fn(&str) -> Reading + Sync> Reader for Told<F> {
        async fn read(&self, url: &str) -> Reading {
            self.asked.lock().unwrap().push((url.to_string(), tokio::time::Instant::now()));
            (self.answer)(url)
        }
    }

    fn price(amount: f64) -> Reading {
        Reading::Price(Priced { amount, currency: "EUR".into(), title: Some("Widget".into()), struck_through: None, out_of_stock: false })
    }

    fn no_price() -> Reading {
        Reading::Unreadable("its data states no product".into())
    }

    fn refused() -> Reading {
        Reading::Refused
    }

    fn failed() -> Reading {
        Reading::Failed("it answered HTTP 503".into())
    }

    /// The daily pace with no wait between two reads of a host, for the
    /// tests that are not about the wait.
    const UNSPACED: Pace = Pace { spacing: Duration::ZERO, ..Pace::DAILY };

    /// A widget read at 329.00 and wanted at 300.00 since `then`, to be
    /// told of in chat 4242.
    async fn watching_since(store: &Store, url: &str, then: chrono::NaiveDateTime) -> PriceWatch {
        let chat = ReplyTo::telegram(4242);
        let id = start(store, &Fixed::priced(329.0), ask(&chat, url, 300.0), then).await.unwrap().started.unwrap();
        store.watch_for_tests(id).unwrap().unwrap()
    }

    fn watch(store: &Store, id: i64) -> PriceWatch {
        store.watch_for_tests(id).unwrap().unwrap()
    }

    /// What is waiting to be sent, as (key, address, body).
    fn waiting(store: &Store) -> Vec<(String, String, String)> {
        store.pending_mirror(crate::mirror::TELEGRAM, 100).unwrap().into_iter().map(|m| (m.turn_key, m.address, m.body)).collect()
    }

    /// A set of counters nobody reads, for the tests that are not about
    /// what `/metrics` says.
    fn counters() -> &'static crate::metrics::Metrics {
        Box::leak(Box::default())
    }

    /// One pass a day for `days` days from `first`, the reader answering
    /// by the day. Returns the day after the last.
    async fn daily<F: Fn(usize) -> Reading + Sync>(store: &Store, first: chrono::NaiveDateTime, days: usize, by_day: F) -> chrono::NaiveDateTime {
        for n in 0..days {
            let reader = Told::by(|_: &str| by_day(n));
            check_due(store, counters(), &reader, first + chrono::Duration::days(n as i64), &UNSPACED).await.unwrap();
        }
        first + chrono::Duration::days(days as i64)
    }

    #[tokio::test]
    async fn a_hit_ends_the_watch_and_is_said_once_with_the_amounts_and_the_link() {
        let (store, _dir) = a_store();
        let now = clock();
        let began = now - chrono::Duration::days(3);
        let w = watching_since(&store, WIDGET, began).await;
        let reader = Told::by(|_: &str| price(289.0));

        let pass = check_due(&store, counters(), &reader, now, &UNSPACED).await.unwrap();

        assert_eq!((pass.due, pass.checked, pass.ended, pass.left), (1, 1, 1, 0));
        let after = watch(&store, w.id);
        assert_eq!(after.ended, Some((now, "hit".to_string())));
        assert_eq!((after.last_price, after.lowest_price, after.first_price), (289.0, 289.0, 329.0));
        assert_eq!(
            waiting(&store),
            vec![(
                format!("watch:{}:hit", w.id),
                "4242".to_string(),
                format!(
                    "Widget is €289.00 at shop.example — under the €300.00 you asked for. It was €329.00 on {}. {WIDGET}",
                    began.format("%-d %B")
                ),
            )]
        );

        // Exactly one: the watch is over, so the passes after it neither
        // read the page nor say anything.
        for later in [1, 21, 48] {
            check_due(&store, counters(), &reader, now + chrono::Duration::hours(later), &UNSPACED).await.unwrap();
        }
        assert_eq!(reader.asked().len(), 1);
        assert_eq!(waiting(&store).len(), 1);
    }

    /// A hit on a watch for "Widget" at 329.00, read now at `amount`
    /// under the name the page gives today.
    async fn hit_under_the_name(now_called: Option<&str>, started_as: Option<&str>, out_of_stock: bool) -> String {
        let (store, _dir) = a_store();
        let now = clock();
        let chat = ReplyTo::telegram(4242);
        let named = |title: Option<&str>, amount: f64, out_of_stock: bool| {
            Reading::Price(Priced { amount, currency: "EUR".into(), title: title.map(tidy_title), struck_through: None, out_of_stock })
        };
        let began = now - chrono::Duration::days(1);
        start(&store, &Fixed::new(named(started_as, 329.0, false)), ask(&chat, WIDGET, 300.0), began).await.unwrap().started.unwrap();
        check_due(&store, counters(), &Told::by(|_: &str| named(now_called, 19.0, out_of_stock)), now, &UNSPACED).await.unwrap();
        let said = waiting(&store);
        assert_eq!(said.len(), 1, "one message whatever the page is called now");
        assert!(said[0].0.ends_with(":hit"));
        said[0].2.clone()
    }

    #[tokio::test]
    async fn a_hit_on_a_page_that_now_names_something_else_says_so_and_asks() {
        // A product is discontinued and its address redirects to the
        // accessory; or the page is reused for the next model. The price
        // read is under the target, and it is not the thing's. Nothing in
        // the data says so but the name, so the name is shown and asked
        // about; the watch still ends, with its one message.
        let said = hit_under_the_name(Some("Widget charging cable"), Some("Widget"), false).await;
        assert!(said.starts_with("Widget is €19.00 at shop.example — under the €300.00 you asked for."), "got: {said}");
        assert!(
            said.ends_with(&format!(
                "The page now calls this \"Widget charging cable\": please check it is still what you meant. {WIDGET}"
            )),
            "got: {said}"
        );

        // The same name is the same name however it is cased and spaced.
        for same in ["Widget", "WIDGET", "  widget\n"] {
            let said = hit_under_the_name(Some(same), Some("Widget"), false).await;
            assert!(!said.contains("now calls this"), "{same:?}: {said}");
        }
        // And at the length a name is kept: a long one read again in
        // full is the name that was cut when the watch began.
        let long = "Widget ".repeat(40);
        let said = hit_under_the_name(Some(&long), Some(&long), false).await;
        assert!(!said.contains("now calls this"), "got: {said}");
        let other = format!("{long} mark two");
        assert!(!hit_under_the_name(Some(&other), Some(&long), false).await.contains("now calls this"), "the difference is past what is kept");

        // With no name on one side there is nothing to set against the
        // other, and nothing is claimed.
        for (now_called, started_as) in [(None, Some("Widget")), (Some("Widget"), None), (None, None)] {
            let said = hit_under_the_name(now_called, started_as, false).await;
            assert!(!said.contains("now calls this"), "{now_called:?} after {started_as:?}: {said}");
        }
    }

    #[tokio::test]
    async fn a_hit_says_when_the_page_says_the_thing_is_out_of_stock() {
        // Said, not waited out: the price is what was asked about, and
        // the person decides what a price on a sold-out page is worth.
        let said = hit_under_the_name(Some("Widget"), Some("Widget"), true).await;
        assert!(
            said.ends_with(&format!("It was €329.00 on {}. The page says it is out of stock. {WIDGET}", (clock() - chrono::Duration::days(1)).format("%-d %B"))),
            "got: {said}"
        );
        assert!(!hit_under_the_name(Some("Widget"), Some("Widget"), false).await.contains("stock"));
        // Both remarks, when both are true.
        let said = hit_under_the_name(Some("Gadget"), Some("Widget"), true).await;
        assert!(said.contains("The page says it is out of stock. The page now calls this \"Gadget\""), "got: {said}");
    }

    #[tokio::test]
    async fn stock_is_read_from_the_product_block_and_the_seller_is_not_read_at_all() {
        let offer = r#""price":"289.00","priceCurrency":"EUR","availability":"https://schema.org/OutOfStock","seller":{"name":"Some Seller"}"#;
        let server = serving(200, &product_page("Widget", offer)).await;
        match read(&server).await {
            Reading::Price(read) => assert!(read.out_of_stock, "{read:?}"),
            other => panic!("{other:?}"),
        }
        let server = serving(200, &widget_at("289.00")).await;
        assert!(matches!(read(&server).await, Reading::Price(Priced { out_of_stock: false, .. })), "a page that says nothing is not sold out");
        let src = include_str!("watch.rs");
        let src = &src[..src.find("#[cfg(test)]").expect("the tests come last")];
        assert!(!src.contains(".seller"), "a watch's messages name no seller");
    }

    #[tokio::test]
    async fn a_price_exactly_at_the_target_is_a_hit_and_says_exactly() {
        let (store, _dir) = a_store();
        let now = clock();
        let w = watching_since(&store, WIDGET, now - chrono::Duration::days(1)).await;
        check_due(&store, counters(), &Told::by(|_: &str| price(300.0)), now, &UNSPACED).await.unwrap();
        assert_eq!(watch(&store, w.id).ended, Some((now, "hit".to_string())));
        assert!(waiting(&store)[0].2.starts_with("Widget is €300.00 at shop.example — exactly the €300.00 you asked for."), "got: {}", waiting(&store)[0].2);
    }

    #[tokio::test]
    async fn a_price_still_over_the_target_is_kept_and_the_page_is_not_read_again_that_day() {
        let (store, _dir) = a_store();
        let now = clock();
        let w = watching_since(&store, WIDGET, now - chrono::Duration::days(1)).await;
        let reader = Told::by(|_: &str| price(310.0));

        let pass = check_due(&store, counters(), &reader, now, &UNSPACED).await.unwrap();
        assert_eq!((pass.due, pass.checked, pass.ended), (1, 1, 0));
        let after = watch(&store, w.id);
        assert_eq!((after.last_price, after.lowest_price, after.last_checked_at, after.ended.clone()), (310.0, 310.0, now, None));
        assert!(waiting(&store).is_empty(), "nothing is said of a watch that goes on");

        // The tick is hourly and the check daily.
        for hour in 1..20 {
            let pass = check_due(&store, counters(), &reader, now + chrono::Duration::hours(hour), &UNSPACED).await.unwrap();
            assert_eq!(pass.due, 0, "hour {hour}");
        }
        assert_eq!(reader.asked().len(), 1);
        check_due(&store, counters(), &reader, now + chrono::Duration::hours(20), &UNSPACED).await.unwrap();
        assert_eq!(reader.asked().len(), 2);

        // The lowest is the lowest ever read, not the latest.
        check_due(&store, counters(), &Told::by(|_: &str| price(320.0)), now + chrono::Duration::hours(41), &UNSPACED).await.unwrap();
        let after = watch(&store, w.id);
        assert_eq!((after.last_price, after.lowest_price), (320.0, 310.0));
    }

    #[tokio::test]
    async fn three_days_running_with_no_price_end_the_watch_with_one_message() {
        let (store, _dir) = a_store();
        let now = clock();
        let w = watching_since(&store, WIDGET, now - chrono::Duration::days(5)).await;

        // Two days, and it is still watched and nothing is said.
        let third = daily(&store, now, 2, |_| no_price()).await;
        let after = watch(&store, w.id);
        assert_eq!((after.unreadable_streak, after.ended.clone(), after.last_price), (2, None, 329.0));
        assert!(waiting(&store).is_empty());

        let fourth = daily(&store, third, 1, |_| no_price()).await;
        assert_eq!(watch(&store, w.id).ended, Some((third, "unreadable".to_string())));
        assert_eq!(
            waiting(&store),
            vec![(
                format!("watch:{}:unreadable", w.id),
                "4242".to_string(),
                format!(
                    "I have stopped watching Widget at shop.example: its page had no price for me at the last 3 daily \
                     checks that read it. The last price I read was €329.00, on {}; you asked to be told at €300.00 or \
                     less. {WIDGET}",
                    w.created_at.format("%-d %B")
                ),
            )]
        );
        daily(&store, fourth, 3, |_| no_price()).await;
        assert_eq!(waiting(&store).len(), 1, "and nothing after it");
    }

    #[tokio::test]
    async fn a_price_read_in_between_starts_the_count_of_unreadable_days_again() {
        let (store, _dir) = a_store();
        let now = clock();
        let w = watching_since(&store, WIDGET, now - chrono::Duration::days(5)).await;
        // No price, no price, a price, no price, no price: never three running.
        daily(&store, now, 5, |day| if day == 2 { price(320.0) } else { no_price() }).await;
        let after = watch(&store, w.id);
        assert_eq!((after.unreadable_streak, after.ended.clone(), after.last_price), (2, None, 320.0));
        assert!(waiting(&store).is_empty());
    }

    #[tokio::test]
    async fn a_price_in_another_currency_than_the_watchs_is_no_price_for_it() {
        // A shop that starts answering in another currency has not come
        // down to a target named in the first: nothing is compared.
        let (store, _dir) = a_store();
        let now = clock();
        let w = watching_since(&store, WIDGET, now - chrono::Duration::days(5)).await;
        let in_dollars = || Reading::Price(Priced { amount: 250.0, currency: "USD".into(), title: None, struck_through: None, out_of_stock: false });
        daily(&store, now, 1, |_| in_dollars()).await;
        let after = watch(&store, w.id);
        assert_eq!((after.unreadable_streak, after.last_price, after.lowest_price, after.ended.clone()), (1, 329.0, 329.0, None));
    }

    #[tokio::test]
    async fn a_day_the_shop_refused_is_not_a_day_the_page_gave_no_price() {
        // A shop that walls the reader for some days has said nothing
        // about the page, and must not end the watch as a page that lost
        // its price would.
        let (store, _dir) = a_store();
        let now = clock();
        let w = watching_since(&store, WIDGET, now - chrono::Duration::days(1)).await;
        let next = daily(&store, now, 6, |_| refused()).await;
        let after = watch(&store, w.id);
        assert_eq!((after.unreadable_streak, after.ended.clone()), (0, None));
        assert_eq!((after.last_price, after.lowest_price), (329.0, 329.0));
        assert_eq!(after.last_checked_at, next - chrono::Duration::days(1), "it was a check all the same, and the shop is not asked again that day");
        // But nothing was read, and the row says since when.
        assert_eq!((after.last_read_at, after.unreached_since), (w.created_at, Some(now)));
        assert!(waiting(&store).is_empty());

        // Nor does it wipe the checks that did count: no price, refused, no
        // price, refused, no price is three readings with none in them.
        let next = daily(&store, next, 4, |day| if day % 2 == 0 { no_price() } else { refused() }).await;
        assert_eq!((watch(&store, w.id).unreadable_streak, watch(&store, w.id).ended), (2, None));
        daily(&store, next, 1, |_| no_price()).await;
        assert_eq!(watch(&store, w.id).ended.map(|(_, why)| why).as_deref(), Some("unreadable"));
    }

    #[tokio::test]
    async fn seven_days_with_no_reading_at_all_end_the_watch_and_say_what_is_true() {
        // The owner's rule, 2026-11-20. Left to run, a watch its shop
        // walls from the second day would be checked for fifty-eight days
        // and read on none of them, and then say the price never came
        // down. It ends after a week instead, saying what happened.
        let (store, _dir) = a_store();
        let now = clock();
        let began = now - chrono::Duration::days(2);
        let w = watching_since(&store, WIDGET, began).await;

        // Seven daily checks span six days: not yet.
        let eighth = daily(&store, now, 7, |_| refused()).await;
        let after = watch(&store, w.id);
        assert_eq!((after.ended.clone(), after.unreached_since), (None, Some(now)));
        assert!(waiting(&store).is_empty());

        daily(&store, eighth, 1, |_| refused()).await;
        assert_eq!(watch(&store, w.id).ended, Some((eighth, "unreached".to_string())));
        assert_eq!(
            waiting(&store),
            vec![(
                format!("watch:{}:unreached", w.id),
                "4242".to_string(),
                format!(
                    "I have stopped watching Widget at shop.example: I have not been able to read its page since {}, \
                     7 days running. The last price I read was €329.00, on {}; you asked to be told at €300.00 or \
                     less. {WIDGET}",
                    now.format("%-d %B"),
                    began.format("%-d %B"),
                ),
            )]
        );
        daily(&store, eighth + chrono::Duration::days(1), 3, |_| refused()).await;
        assert_eq!(waiting(&store).len(), 1, "and nothing after it");
    }

    #[tokio::test]
    async fn an_outage_is_not_a_page_with_no_price_and_ends_nothing_in_forty_hours() {
        // Three checks that fail are forty hours of somebody's outage,
        // the shop's or Scout's own. Counted as a page with no price they
        // ended every watch they touched, each saying its page gave none.
        let (store, _dir) = a_store();
        let now = clock();
        let w = watching_since(&store, WIDGET, now - chrono::Duration::days(2)).await;
        for check in 0..3 {
            let at = now + chrono::Duration::hours(20 * check);
            check_due(&store, counters(), &Told::by(|_: &str| failed()), at, &UNSPACED).await.unwrap();
        }
        let after = watch(&store, w.id);
        assert_eq!((after.unreadable_streak, after.ended.clone(), after.unreached_since), (0, None, Some(now)));
        assert!(waiting(&store).is_empty());

        // It is a week with no reading that ends it, failures and
        // refusals alike and mixed, and the message is the week's.
        let next = daily(&store, now + chrono::Duration::days(3), 5, |day| if day % 2 == 0 { refused() } else { failed() }).await;
        assert_eq!(watch(&store, w.id).ended, Some((next - chrono::Duration::days(1), "unreached".to_string())));
        let said = waiting(&store);
        assert_eq!(said.len(), 1);
        assert!(said[0].2.contains("I have not been able to read its page since"), "got: {}", said[0].2);
        assert!(!said[0].2.contains("no price for me"), "got: {}", said[0].2);
    }

    #[tokio::test]
    async fn a_reading_of_any_kind_starts_the_week_again() {
        let (store, _dir) = a_store();
        let now = clock();
        let w = watching_since(&store, WIDGET, now - chrono::Duration::days(2)).await;
        // Six days shut out, then the page is read: once with a price,
        // once with none. Either is a reading, and the week starts over.
        let next = daily(&store, now, 6, |_| refused()).await;
        let next = daily(&store, next, 1, |_| price(320.0)).await;
        assert_eq!(watch(&store, w.id).unreached_since, None);
        let again = next;
        let next = daily(&store, next, 6, |_| refused()).await;
        assert_eq!(watch(&store, w.id).unreached_since, Some(again));
        let next = daily(&store, next, 1, |_| no_price()).await;
        let after = watch(&store, w.id);
        assert_eq!((after.unreached_since, after.unreadable_streak, after.ended.clone()), (None, 1, None));
        daily(&store, next, 6, |_| refused()).await;
        assert_eq!(watch(&store, w.id).ended, None);
        assert!(waiting(&store).is_empty());
    }

    #[tokio::test]
    async fn at_sixty_days_a_watch_ends_with_the_lowest_price_it_read() {
        let (store, _dir) = a_store();
        let now = clock();
        let began = now - chrono::Duration::days(60);
        let w = watching_since(&store, WIDGET, began).await;
        assert_eq!(w.expires_at, now, "sixty days to the second");
        // A dip on the tenth day that did not reach the target.
        check_due(&store, counters(), &Told::by(|_: &str| price(305.0)), began + chrono::Duration::days(10), &UNSPACED).await.unwrap();
        // The day before its last it is a watch like any other.
        check_due(&store, counters(), &Told::by(|_: &str| price(320.0)), now - chrono::Duration::days(1), &UNSPACED).await.unwrap();
        assert_eq!(watch(&store, w.id).ended, None);
        assert!(waiting(&store).is_empty());

        let pass = check_due(&store, counters(), &Told::by(|_: &str| price(318.0)), now, &UNSPACED).await.unwrap();
        assert_eq!((pass.checked, pass.ended), (1, 1));
        assert_eq!(watch(&store, w.id).ended, Some((now, "expired".to_string())));
        assert_eq!(
            waiting(&store),
            vec![(
                format!("watch:{}:expired", w.id),
                "4242".to_string(),
                format!(
                    "My 60 days of watching Widget at shop.example are up, and I did not see it at €300.00 or less. \
                     The lowest price I read was €305.00; the last time I read a price was {}. {WIDGET}",
                    now.format("%-d %B")
                ),
            )]
        );
        check_due(&store, counters(), &Told::by(|_: &str| price(289.0)), now + chrono::Duration::days(1), &UNSPACED).await.unwrap();
        assert_eq!(waiting(&store).len(), 1, "an ended watch is not hit");
    }

    #[tokio::test]
    async fn a_watch_that_expires_unread_says_when_it_last_read_a_price_and_claims_nothing_since() {
        // Shut out for its last five days, short of the week that would
        // have ended it. What it may say is what it read and when: not
        // that the price stayed where it was.
        let (store, _dir) = a_store();
        let now = clock();
        let began = now - chrono::Duration::days(60);
        let w = watching_since(&store, WIDGET, began).await;
        let last_read = now - chrono::Duration::days(6);
        check_due(&store, counters(), &Told::by(|_: &str| price(310.0)), last_read, &UNSPACED).await.unwrap();
        daily(&store, now - chrono::Duration::days(5), 6, |_| refused()).await;

        assert_eq!(watch(&store, w.id).ended, Some((now, "expired".to_string())));
        let said = waiting(&store);
        assert_eq!(said.len(), 1);
        assert_eq!(
            said[0].2,
            format!(
                "My 60 days of watching Widget at shop.example are up, and I did not see it at €300.00 or less. \
                 The lowest price I read was €310.00; the last time I read a price was {}. {WIDGET}",
                last_read.format("%-d %B")
            )
        );
        assert!(!said[0].2.contains("did not come down"), "nothing was read for five days: {}", said[0].2);
    }

    #[tokio::test]
    async fn the_last_reading_decides_how_a_watch_past_its_sixty_days_ends() {
        // One message whichever: a price that got there on the last day
        // is a hit, and anything else, a shop's refusal included, is the
        // end of the sixty days. Checked an hour before or not, a watch
        // past its end is read once more and ended.
        for (last, why) in [(price(289.0), "hit"), (price(310.0), "expired"), (no_price(), "expired"), (refused(), "expired")] {
            let (store, _dir) = a_store();
            let now = clock();
            let w = watching_since(&store, WIDGET, now - chrono::Duration::days(60) - chrono::Duration::minutes(30)).await;
            check_due(&store, counters(), &Told::by(|_: &str| price(315.0)), now - chrono::Duration::hours(1), &UNSPACED).await.unwrap();
            assert_eq!(watch(&store, w.id).ended, None);

            check_due(&store, counters(), &Told::by(|_: &str| last.clone()), now, &UNSPACED).await.unwrap();
            assert_eq!(watch(&store, w.id).ended, Some((now, why.to_string())), "after {last:?}");
            let said = waiting(&store);
            assert_eq!(said.len(), 1, "after {last:?}");
            assert_eq!(said[0].0, format!("watch:{}:{why}", w.id));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reads_of_one_host_are_five_seconds_apart_and_another_host_is_not_kept_waiting() {
        let (store, _dir) = a_store();
        let now = clock();
        let then = now - chrono::Duration::days(1);
        for url in ["https://www.shop.example/p/1", "https://shop.example/p/2", "https://other.example/p/1", "https://shop.example/p/3"] {
            watching_since(&store, url, then).await;
        }
        let reader = Told::by(|_: &str| price(310.0));
        let began = tokio::time::Instant::now();

        check_due(&store, counters(), &reader, now, &Pace::DAILY).await.unwrap();

        let at: Vec<(String, u64)> = reader.asked().into_iter().map(|(url, when)| (shop_of(&url), (when - began).as_secs())).collect();
        assert_eq!(
            at,
            vec![
                ("shop.example".to_string(), 0),
                // With the www or without, it is one shop's server.
                ("shop.example".to_string(), 5),
                ("other.example".to_string(), 5),
                ("shop.example".to_string(), 10),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_that_never_answers_costs_its_watch_a_day_and_the_pass_goes_on() {
        struct Stuck;
        impl Reader for Stuck {
            async fn read(&self, url: &str) -> Reading {
                if url.contains("stuck.example") {
                    std::future::pending::<()>().await;
                }
                price(310.0)
            }
        }
        let (store, _dir) = a_store();
        let now = clock();
        let then = now - chrono::Duration::days(1);
        let stuck = watching_since(&store, "https://stuck.example/p/1", then).await;
        let fine = watching_since(&store, "https://shop.example/p/1", then).await;
        let began = tokio::time::Instant::now();

        let pass = check_due(&store, counters(), &Stuck, now, &Pace::DAILY).await.unwrap();

        assert_eq!(began.elapsed(), READ_BUDGET, "given up on after the one reading's budget");
        assert_eq!((pass.due, pass.checked, pass.left), (2, 2, 0));
        let after = watch(&store, stuck.id);
        assert_eq!((after.last_checked_at, after.ended.clone()), (now, None));
        // Nothing was read, which is not a page that had no price.
        assert_eq!((after.unreadable_streak, after.unreached_since), (0, Some(now)));
        assert_eq!(watch(&store, fine.id).last_price, 310.0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_pass_out_of_time_leaves_the_rest_due_for_the_next_tick() {
        struct Slow;
        impl Reader for Slow {
            async fn read(&self, _url: &str) -> Reading {
                tokio::time::sleep(Duration::from_secs(60)).await;
                price(310.0)
            }
        }
        let (store, _dir) = a_store();
        let now = clock();
        // Five watches, the first the longest unchecked.
        let mut ids = Vec::new();
        for n in 0..5 {
            let then = now - chrono::Duration::days(2) + chrono::Duration::minutes(n);
            ids.push(watching_since(&store, &format!("https://shop{n}.example/p"), then).await.id);
        }
        let pace = Pace { pass_budget: Duration::from_secs(150), ..Pace::DAILY };
        let began = tokio::time::Instant::now();

        let pass = check_due(&store, counters(), &Slow, now, &pace).await.unwrap();

        // Three minute-long reads began inside two and a half minutes.
        assert_eq!(began.elapsed(), Duration::from_secs(180));
        assert_eq!((pass.due, pass.checked, pass.left), (5, 3, 2));
        assert_eq!(pass.took, Duration::from_secs(180), "and the pass says how long it took, for the log");
        let checked: Vec<bool> = ids.iter().map(|id| watch(&store, *id).last_checked_at == now).collect();
        assert_eq!(checked, [true, true, true, false, false], "the longest unchecked went first");
        // And the next tick takes up where this one stopped.
        let next = check_due(&store, counters(), &Slow, now + chrono::Duration::hours(1), &pace).await.unwrap();
        assert_eq!((next.due, next.checked, next.left), (2, 2, 0));
    }

    #[tokio::test]
    async fn a_page_the_reader_panics_on_costs_that_watch_its_day_and_the_pass_goes_on() {
        // The parser reads whatever a shop serves. A page it panics on
        // must not end the pass, and must not be first in line again.
        struct Poisoned;
        impl Reader for Poisoned {
            async fn read(&self, url: &str) -> Reading {
                if url.contains("poison") {
                    panic!("byte index 3 is not a char boundary");
                }
                price(310.0)
            }
        }
        let (store, _dir) = a_store();
        let now = clock();
        let poisoned = watching_since(&store, "https://shop.example/p/poison", now - chrono::Duration::days(2)).await;
        let fine = watching_since(&store, "https://other.example/p/1", now - chrono::Duration::days(1)).await;
        let metrics = counters();

        let pass = check_due(&store, metrics, &Poisoned, now, &UNSPACED).await.unwrap();

        assert_eq!((pass.due, pass.checked, pass.ended), (2, 2, 0));
        assert_eq!(watch(&store, fine.id).last_price, 310.0, "the watch after it was read");
        // A read that panicked is a request that came to nothing: no
        // price changed, no count of a page without one, and the week's
        // clock running, which is what ends it if it never reads.
        let after = watch(&store, poisoned.id);
        assert_eq!((after.last_checked_at, after.last_price, after.unreadable_streak), (now, 329.0, 0));
        assert_eq!((after.unreached_since, after.ended.clone()), (Some(now), None));
        assert!(metrics.render().contains("scout_price_watch_checks_total{outcome=\"failed\"} 1\n"), "{}", metrics.render());

        // The next tick does not start with it, or read it at all.
        let next = check_due(&store, metrics, &Poisoned, now + chrono::Duration::hours(1), &UNSPACED).await.unwrap();
        assert_eq!(next.due, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_watch_whose_read_never_came_back_is_not_first_in_line_at_the_next_pass() {
        // The process dies mid-read, or the task is dropped: nothing of
        // the check is written after the read. The stamp before it is
        // what keeps the same page from being the first one asked for at
        // every start.
        struct Never;
        impl Reader for Never {
            async fn read(&self, _url: &str) -> Reading {
                std::future::pending().await
            }
        }
        let (store, _dir) = a_store();
        let now = clock();
        let first = watching_since(&store, "https://shop.example/p/1", now - chrono::Duration::days(2)).await;
        let second = watching_since(&store, "https://other.example/p/1", now - chrono::Duration::days(1)).await;
        let forever = Pace { read_budget: Duration::from_secs(86_400), ..UNSPACED };

        let cut = tokio::time::timeout(Duration::from_secs(1), check_due(&store, counters(), &Never, now, &forever)).await;
        assert!(cut.is_err(), "the pass was dropped in the middle of its first read");

        assert_eq!(watch(&store, first.id).last_checked_at, now);
        assert_eq!(watch(&store, second.id).last_checked_at, second.last_checked_at, "not reached");
        let due = store.due_watches(now + chrono::Duration::hours(1), now - chrono::Duration::hours(19)).unwrap();
        assert_eq!(due.iter().map(|w| w.id).collect::<Vec<_>>(), vec![second.id]);
    }

    #[tokio::test]
    async fn a_watch_stopped_before_its_turn_in_a_pass_is_not_read() {
        // Listed as due when the pass began and stopped while it read the
        // watches before: the shop is not asked about a watch that is over.
        let (store, _dir) = a_store();
        let now = clock();
        let first = watching_since(&store, "https://shop.example/p/1", now - chrono::Duration::days(2)).await;
        let second = watching_since(&store, "https://other.example/p/2", now - chrono::Duration::days(1)).await;
        let (theirs, stopped) = (store.clone(), second.id);
        let reader = Told::by(move |url: &str| {
            if url.ends_with("/p/1") {
                theirs.stop_watch(1, stopped, now).unwrap().expect("live until now");
            }
            price(310.0)
        });

        let pass = check_due(&store, counters(), &reader, now, &UNSPACED).await.unwrap();

        assert_eq!((pass.due, pass.checked), (2, 1));
        assert_eq!(reader.asked().len(), 1, "only the first: {:?}", reader.asked());
        assert_eq!(watch(&store, first.id).last_price, 310.0);
    }

    #[tokio::test]
    async fn a_watch_stopped_while_its_page_was_being_read_is_sent_nothing() {
        let (store, _dir) = a_store();
        let now = clock();
        let w = watching_since(&store, WIDGET, now - chrono::Duration::days(1)).await;
        // The page is read with the store's lock let go: its owner stops
        // the watch in that moment, and the read comes back with a hit.
        let (theirs, id) = (store.clone(), w.id);
        let reader = Told::by(move |_: &str| {
            theirs.stop_watch(1, id, now).unwrap().expect("live until now");
            price(289.0)
        });

        let pass = check_due(&store, counters(), &reader, now, &UNSPACED).await.unwrap();

        assert_eq!((pass.due, pass.checked, pass.ended), (1, 0, 0));
        assert_eq!(watch(&store, w.id).ended, Some((now, "stopped".to_string())));
        assert!(waiting(&store).is_empty());
    }

    #[tokio::test]
    async fn a_shop_that_refuses_is_asked_once_in_a_pass_however_many_watches_it_has() {
        // Through the real reader, which is where a pass remembers a host
        // that said no: three watches on one shop, one request.
        let shop = serving(200, NOTICE).await;
        let (store, _dir) = a_store();
        let now = clock();
        let mut ids = Vec::new();
        for n in 0..3 {
            ids.push(watching_since(&store, &format!("{}/p/{n}", shop.uri()), now - chrono::Duration::days(1)).await.id);
        }
        let reader = PageReader::for_a_pass(OpenWeb::exempting(*shop.address()), None);

        let pass = check_due(&store, counters(), &reader, now, &UNSPACED).await.unwrap();

        assert_eq!(shop.received_requests().await.unwrap().len(), 1, "the shop said no once and was not asked again");
        assert_eq!((pass.due, pass.checked, pass.ended), (3, 3, 0));
        for id in ids {
            let after = watch(&store, id);
            assert_eq!((after.unreadable_streak, after.last_checked_at, after.ended.clone()), (0, now, None), "watch {id}");
        }
        assert!(waiting(&store).is_empty());
        // Nor an hour on: each watch was checked today, refused or not.
        let next = check_due(&store, counters(), &reader, now + chrono::Duration::hours(1), &UNSPACED).await.unwrap();
        assert_eq!(next.due, 0);
        assert_eq!(shop.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_pass_reads_a_real_page_and_queues_the_hit_for_the_channel_to_send() {
        // The whole road with nothing stood in for but the shop: the page
        // over HTTP, the product block, the comparison, the outbox.
        let shop = serving(200, &widget_at("289.00")).await;
        let (store, _dir) = a_store();
        let now = clock();
        let url = format!("{}/p/widget?variant=2", shop.uri());
        let w = watching_since(&store, &url, now - chrono::Duration::days(1)).await;
        let reader = PageReader::for_a_pass(OpenWeb::exempting(*shop.address()), None);

        let pass = check_due(&store, counters(), &reader, now, &UNSPACED).await.unwrap();

        assert_eq!((pass.checked, pass.ended), (1, 1));
        let asked = shop.received_requests().await.unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].url.query(), Some("variant=2"), "the address the watch was started on, query and all");
        let said = waiting(&store);
        assert_eq!(said.len(), 1);
        assert_eq!(said[0].0, format!("watch:{}:hit", w.id));
        assert!(said[0].2.starts_with("Widget is €289.00 at 127.0.0.1 — under the €300.00 you asked for."), "got: {}", said[0].2);
        assert!(said[0].2.ends_with(&url), "got: {}", said[0].2);
    }

    #[tokio::test]
    async fn the_page_counts_each_check_by_how_it_went_and_each_ending_by_why() {
        let (store, _dir) = a_store();
        let now = clock();
        let then = now - chrono::Duration::days(1);
        let mut ids = std::collections::HashMap::new();
        for page in ["hit", "over", "gone", "walled", "busy", "stopped"] {
            let url = format!("https://shop.example/p/{page}");
            ids.insert(page, watching_since(&store, &url, then).await.id);
        }
        // "gone" has had no price for two days already.
        let gone = watch(&store, ids["gone"]);
        let two_days = crate::store::WatchCheck {
            id: gone.id,
            now: then,
            last_price: 329.0,
            lowest_price: 329.0,
            last_read_at: then,
            unreadable_streak: 2,
            unreached_since: None,
            ended: None,
        };
        assert!(store.record_watch_check(&two_days).unwrap());

        let (theirs, stopped) = (store.clone(), ids["stopped"]);
        let reader = Told::by(move |url: &str| match url.rsplit('/').next().unwrap() {
            "hit" => price(289.0),
            "over" => price(310.0),
            "gone" => no_price(),
            "walled" => refused(),
            "busy" => failed(),
            _ => {
                theirs.stop_watch(1, stopped, now).unwrap().expect("live until now");
                price(289.0)
            }
        });
        let metrics = counters();

        let pass = check_due(&store, metrics, &reader, now, &UNSPACED).await.unwrap();

        assert_eq!((pass.due, pass.checked, pass.ended), (6, 5, 2));
        let page = metrics.render();
        for series in [
            // The stopped one's page was read too: a check is a request
            // made, whatever became of the watch meanwhile.
            "scout_price_watch_checks_total{outcome=\"read\"} 3",
            "scout_price_watch_checks_total{outcome=\"unreadable\"} 1",
            "scout_price_watch_checks_total{outcome=\"refused\"} 1",
            // A request that failed is neither: nothing refused it.
            "scout_price_watch_checks_total{outcome=\"failed\"} 1",
            "scout_price_watches_ended_total{why=\"hit\"} 1",
            "scout_price_watches_ended_total{why=\"unreadable\"} 1",
        ] {
            assert!(page.contains(&format!("{series}\n")), "{series} is missing:\n{page}");
        }
        // The pass ended two watches and counted two: the one its owner
        // stopped is the stop's to count, not the pass's.
        assert_eq!(page.matches("scout_price_watches_ended_total{").count(), 2, "{page}");
        // And how many are running now: six, less the two it ended and
        // the one that was stopped.
        assert!(page.contains("scout_price_watches_live 3\n"), "{page}");

        // Sixty days on, the three still live expire.
        let later = now + chrono::Duration::days(60);
        check_due(&store, metrics, &Told::by(|_: &str| price(310.0)), later, &UNSPACED).await.unwrap();
        assert!(metrics.render().contains("scout_price_watches_ended_total{why=\"expired\"} 3\n"), "{}", metrics.render());
    }

    #[test]
    fn one_reading_fits_between_what_its_parts_may_take_and_a_runs_patience() {
        // Over the plain request's 30 s and a render's 45 s together, or a
        // page still being rendered would be given up on; under the 90 s
        // after which a run with a silent tool is cut for stalled, or a
        // person asking for a watch would lose the whole answer to it.
        let parts = Duration::from_secs(30) + crate::tools::browser::RENDER_TIMEOUT;
        assert!(READ_BUDGET > parts, "{READ_BUDGET:?} is not over {parts:?}");
        let run = include_str!("run.rs");
        assert!(
            run.contains("const STREAM_STALL: std::time::Duration = std::time::Duration::from_secs(90);"),
            "a run's patience with a silent tool has changed: set READ_BUDGET against the new one"
        );
        assert!(READ_BUDGET < Duration::from_secs(90));
        // And the page reader's own client is the 30 s this counts on.
        let core = include_str!("core.rs");
        assert!(core.contains("let web = crate::net_guard::OpenWeb::new(Duration::from_secs(30))?;"), "the open web's timeout has changed");
    }

    #[test]
    fn the_readme_says_what_a_watch_is_in_the_numbers_held_here() {
        // The README spells the numbers out, and nothing else would notice
        // one of them changing here and not there.
        let readme = include_str!("../../../README.md");
        let entry = readme.split("- **Price watches.**").nth(1).expect("the README says what a price watch is");
        let entry = entry.split("\n- ").next().unwrap().split_whitespace().collect::<Vec<_>>().join(" ");
        let spelt = |n: i64| match n {
            3 => "three",
            7 => "seven",
            10 => "ten",
            60 => "sixty",
            other => panic!("the README has no word yet for {other}: write it there and here"),
        };
        for said in [
            format!("{} at a time", spelt(MAX_LIVE)),
            format!("at {} days", spelt(DAYS)),
            format!("no price at {} checks running", spelt(UNREADABLE_CHECKS)),
            format!("nothing at all could be read for {} days running", spelt(UNREACHED_DAYS)),
            "once a day".to_string(),
            "exactly one message".to_string(),
            "no model in the loop".to_string(),
            "says what it read".to_string(),
        ] {
            assert!(entry.to_lowercase().contains(&said), "the README's entry does not say {said:?}: {entry}");
        }
        // And what a watch does not promise is said where the limits are.
        let limits = readme.split("## Honest limitations").nth(1).expect("the README has its limitations");
        assert!(limits.contains("**A watch reads the page's own number, once a day.**"));
    }

    #[test]
    fn a_check_costs_http_and_nothing_else() {
        // No model is asked whether a price is under a target. A source
        // assertion, as `every_side_call_is_built_on_the_side_model_and_nowhere_else`
        // is: this file reaches no model and holds nothing one could be
        // reached through.
        let src = include_str!("watch.rs");
        let src = &src[..src.find("#[cfg(test)]").expect("the tests come last")];
        for forbidden in ["AgentDeps", "side_call", ".llm", ".agent(", "completion", "Prompt"] {
            assert!(!src.contains(forbidden), "watch.rs reaches for a model: {forbidden}");
        }
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
    fn a_products_name_cannot_carry_a_link_a_command_or_a_mention_into_a_message() {
        // The name is the page's text and goes into a Telegram message as
        // it is. Telegram makes a link of an address, a tappable command
        // of `/word` and a mention of `@word`, and a shop's page, or
        // whoever wrote one, would be choosing what a message from Scout
        // links to.
        for (name, kept) in [
            ("Widget https://evil.example/claim now", "Widget now"),
            ("Widget www.evil.example/claim", "Widget"),
            ("Widget evil.example/claim", "Widget evil example/claim"),
            ("Widget t.me/joinchat/abc", "Widget t me/joinchat/abc"),
            ("Shop24.example Widget", "Shop24 example Widget"),
            ("/start Widget", "start Widget"),
            ("Widget by @someone", "Widget by someone"),
            ("Widget a@b.example", "Widget a b example"),
            ("HTTP://EVIL.EXAMPLE Widget", "Widget"),
        ] {
            assert_eq!(tidy_title(name), kept, "from {name:?}");
        }
        // What a name really has in it is left alone: a size, a version,
        // a fraction, a slash between two things.
        for name in ["Widget 2.0", "Widget 1.5 kg", "Widget 3,5 mm / 2 m", "Widget A/B 1/2", "Widget (v2.1) 50%"] {
            assert_eq!(tidy_title(name), name);
        }
    }

    #[test]
    fn a_price_that_is_no_amount_and_a_struck_amount_in_another_currency_are_not_read() {
        // `fetch_page` writes neither today: its prices are above zero
        // and a struck-through amount is in its offer's own currency. A
        // block is not trusted to stay that way by the reader of it.
        let block = |price: &str, struck: Option<&str>| PageProduct {
            name: Some("Widget".into()),
            price: Some(price.into()),
            struck_through_price: struck.map(str::to_string),
            availability: None,
            seller: None,
        };
        for price in ["0.00 EUR", "-5.00 EUR", "NaN EUR", "inf EUR", "free EUR"] {
            assert_eq!(
                priced(Some(block(price, None))),
                Reading::Unreadable("its data states a price that is not an amount".to_string()),
                "for {price}"
            );
        }
        let struck = |stated: &str| match priced(Some(block("30.59 EUR", Some(stated)))) {
            Reading::Price(read) => read.struck_through,
            other => panic!("{other:?}"),
        };
        assert_eq!(struck("35.99 EUR"), Some(35.99));
        assert_eq!(struck("35.99 USD"), None, "another currency's amount is no second price for this one");
        for stated in ["0.00 EUR", "-1.00 EUR", "35.99", "soon EUR"] {
            assert_eq!(struck(stated), None, "for {stated}");
        }
    }

    #[test]
    fn the_pace_production_runs_at_is_the_one_the_design_and_the_notes_give() {
        assert_eq!(Pace::DAILY.spacing, Duration::from_secs(5));
        assert_eq!(Pace::DAILY.read_budget, READ_BUDGET);
        assert_eq!(Pace::DAILY.pass_budget, Duration::from_secs(30 * 60), "half the hour between two ticks");
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
