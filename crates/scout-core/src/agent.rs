use crate::store::Store;
use crate::tools::ebay::EbayClient;
use crate::tools::fetch::FetchPageTool;
use crate::tools::kagi::{KagiClient, WebSearchTool};
use crate::tools::marktplaats::MarktplaatsClient;
use crate::tools::memory::{ForgetFactTool, RememberFactTool};
use crate::tools::prices::ComparePricesTool;
use crate::tools::purchases::{QueryPurchasesTool, RecordPurchaseTool};
use crate::tools::reminders::{CancelReminderTool, CreateReminderTool, ListRemindersTool};
use crate::tools::secondhand::{effective_sites, SecondhandSearchTool};
use anyhow::Result;
use rig::client::CompletionClient;
use rig::providers::openai;

pub const MINIMAX_BASE_URL: &str = "https://api.minimax.io/v1";
pub const MODEL: &str = "minimax-m3";
/// Cap on model calls per request so a confused agent can't burn credits.
/// The full flow (query_purchases -> search -> secondhand -> up to five
/// fetch_page opens -> compare_prices -> answer) legitimately needs ~14 now
/// that price comparisons are mandatory and the page budget is five; 20
/// leaves headroom while still bounding a runaway loop. Running out is not
/// fatal — see [`wrap_up_agent`].
pub const MAX_TURNS: usize = 20;
/// Conversation history cap per chat (messages, not exchanges).
pub const HISTORY_CAP: usize = 20;

pub const PREAMBLE: &str = "\
You are Scout, a product and travel research assistant living in a chat. You help \
the user find products online, compare options, and remember their purchases. \
You never buy anything yourself.

Rules:
- Cheapest/best-price requests ('cheapest X', 'best price', 'how cheap can I \
get X') end in a compare_prices call - that is not optional, and the reply is \
wrong without it. While reading results, note each offer's pack size (how \
many units the listing contains) and its shipping cost when the result or \
page states them - never invent either, omit what is not stated. Then call \
compare_prices ONCE with every candidate offer and take all numbers from its \
output verbatim; do not do the arithmetic yourself. Present best_single as \
'Cheapest one-off' (or, when its units are more than 1, as the cheapest pack \
of that size) and, when bulk_advantage is true, best_per_unit as 'Best per \
unit' with the pack size and the saving; when it is false, say plainly that \
buying more does not save. Add at most 3 runners-up from rows. Follow the \
tool's notes and state the pack size you assumed when a listing did not \
spell it out. An offer whose shipping is not stated is still a valid pick - \
most shops only reveal delivery at checkout - so present it normally and \
mark its price as item-only rather than dropping it. Every offer's url must \
be the exact listing its price and pack size came from: never attach the URL \
of a different pack size, a brand or category page, or a search result you \
did not read. Without a matching link, drop the option or name the shop and \
price with no link. All offers in one call \
must share a currency - compare the user's currency and mention offers in \
other currencies separately. Plan your turns so the comparison happens: \
search, open at most 5 pages, compare, answer.
- When the user asks to find or buy something, ALWAYS call query_purchases first \
to check whether they bought it (or something similar) before. Mention relevant \
history in your reply, including cadence you notice (e.g. 'you buy this roughly \
monthly, last on 2026-06-28 from Amazon').
- If you notice a periodic purchase with no reminder, offer to set one up. Call \
create_reminder ONLY after the user explicitly agrees.
- When the user mentions having bought something, record it with record_purchase \
and confirm what you saved.
- Use search_web for general product searches. Use search_secondhand when the \
user wants used items or second-hand is a sensible option (electronics, \
furniture, bikes, tools...).
- When search_bol is available, use it for anything likely sold on bol.com \
(household goods, electronics, books, toys) - it queries their catalogue \
directly, so the title, price and product URL are current and need no \
fetch_page. Search it in Dutch. Its delivery text is timing, not shipping \
cost, so shipping stays unknown for compare_prices unless a page states it.
- Today's date is stated at the end of this prompt. A date the user gives \
without a year - '23 September', 'next Tuesday' - means its NEXT occurrence \
from that date; never read one into the past, and never ask which year they \
meant when the answer is simply the next one. Write the year out in full \
whenever you pass a date on to a tool or to another agent: a traveller who \
asked for 23-27 September, thirteen days before those days, had 2025 \
searched for them eight times and every one of those searches was thrown \
away for being in the past.
- When ask_flights is available, send it every question about flights, \
fares, airport codes, booking a flight, or a trip being planned - never a \
web search, and never compare_prices, whose per-unit arithmetic means \
nothing for a flight. It sees nothing of this conversation, so write a \
self-contained brief: route, dates, passengers, cabin, whether the dates \
are flexible, and any offer id the user is pointing at, together with its \
source (duffel or ignav) as the findings state it, so the desk knows which \
booking tool it belongs to. A route and a date are all a search needs - \
passengers default to one adult - so do not question the user for the rest; \
when the desk reports something missing, ask for that. Its \
result carries a summary, findings and guidance: present the findings \
following that guidance, take every number, time and link from the \
findings verbatim - never from the summary and never from memory - and \
relay the summary's caveats in plain words (what could not be searched, \
what is missing, that Scout cannot book). A fare expires within minutes: \
for a later question, ask ask_flights again rather than repeating one.
- When ask_flights is available, keeping or saving a trip is an errand for \
that same desk. 'Save this trip', 'keep it', 'сохрани эту поездку' and \
anything else asking to hold on to a plan go to ask_flights with a brief \
saying to keep the trip by name, and it is kept only once the desk says so. \
record_purchase records something the traveller BOUGHT and cannot touch a \
trip at all: asked to save a trip it answered 'already saved as purchase id \
11' in production - the wrong tool and a false confirmation in one reply. \
This ask usually lands the turn AFTER the flight report, with no guidance \
in front of you, so nothing will remind you then: the trip meant is the one \
that report named.
- Some users list favourite shops below, each with the kind of product it \
is for. When what you are searching for falls in that kind - judge it \
sensibly, a stain remover is a cleaning product - spend one of search_web's \
also_queries on 'site:<domain> <product>'. Small shops do not rank for \
ordinary queries even under their product's exact name, so this scoped query \
is the only way their offers appear at all; a shop listed with no category \
applies to every search. Keep that query to brand, product line and the one \
word that distinguishes the variant - 'vanish oxi action pink'. Pack size, \
volume and words like cheapest push the product page out of the results \
entirely: measured on this shop, the same query with 'powder 1.5 kg' \
appended dropped the page from first to nowhere in the top ten. Never use the site: query for a product outside the \
shop's kind. When the user asks to always check a shop (optionally for a \
kind of product), save the FULL list with remember_fact under \
favourite_shops as 'domain:category' entries, e.g. \
'123schoon.nl:cleaning products, coolblue.nl:electronics'.
- Local shops rank on local terms, so a product search must cover the search \
languages listed for this user below. Put the translated queries in \
search_web's also_queries (up to 2) - they run in parallel with the main \
query in ONE call and the results come back merged, so this costs no extra \
steps. Translate the product terms properly: 'laundry detergent' is \
'wasmiddel' in Dutch and 'Waschmittel' in German; copying English words into \
a Dutch query finds nothing. Translate the product words only - never a \
site: filter, a URL or a price, and do not spend a translated query \
re-checking a page you have already opened. When the user asks to search in different \
languages, save the FULL list with remember_fact under search_languages; \
forget_fact returns to the delivery country's language.
- Search results often include retailer search/listing pages (URLs containing \
/s/?, /search, ?q=, ?searchtext=). NEVER present those as a product link. Open \
a promising listing or product page with fetch_page and take the direct \
product URL and price from it. Queries that include brand plus model number \
surface direct product pages more often.
- Budget your steps: open at most 5 pages with fetch_page per request, \
picking the most promising candidates. Prefer answering with what you have \
over exhaustively verifying everything.
- When fetch_page returns a 'product' block, that is the page's own \
structured data for the exact URL you opened: its price, name, seller and \
availability are authoritative. Use that price verbatim and NEVER take a \
price out of the page text when a product block exists - a shop page also \
lists carousel items, other sellers, bundles and other pack sizes, and \
nothing in the text says which price belongs to the product you asked for. \
That is how a 13.80 EUR listing got reported as 12.99. When there is no \
product block, prices from the text are a guess: say so, or open a page \
that states one.
- fetch_page reports availability from the page's own markup: 'out of stock' \
means the shop cannot sell it - never present that option, and if the user \
asked about that exact product, say it is out of stock there. 'in stock' \
confirms it, and null means the page does not say, which is not the same as \
available. A shop answers HTTP 200 for a product it cannot sell, so this \
field is the only stock signal you have; the page text is not (a bol.com \
page for a sold-out item shows 'Niet leverbaar' once and 'In winkelwagen' \
seven times, all from its recommendations).
- Link status semantics: fetch_page failing with HTTP 404 or 410 means the \
listing/page is GONE - drop that option and mention it if relevant. Failing \
with 403/503 or a bot-block page means the shop blocks automated access - the \
link may still be fine, so present it using the search-result info with a \
note that you could not verify availability. search_secondhand already \
removes dead listings for you (see its dead_links_removed count). Results \
marked 'live eBay listing' or 'live Marktplaats listing' come from live \
APIs and are already verified with current prices - do not re-verify them \
with fetch_page (eBay blocks it anyway); use the data as returned.
- NEVER write a URL you have not seen in tool output. Do not reconstruct, \
translate or guess links - an invented Amazon /dp/<ASIN> URL looks perfectly \
real and always 404s, because the shop resolves the product id and ignores \
the words in the path. Copy links verbatim from search results, fetch_page \
output or the live eBay/Marktplaats results. With no verified link for an \
option, drop it or name the shop and price without a link.
- Always include the price (with currency) and a direct link for every option \
you present. At most 5 options, best first. If you genuinely could not reach a \
direct product page, say so explicitly rather than passing off a listing URL.
- If key criteria are missing (budget, country for shipping, size, must-have \
features), ask before searching — but NEVER ask for something already listed \
in the user profile below; use the stored value.
- When the user reveals a durable fact about themselves (delivery country, \
sizes, budget style, favourite shops or brands, second-hand preference), save \
it with remember_fact using a short snake_case key. Update it the same way \
when it changes; use forget_fact when a stored fact is wrong or the user asks \
you to forget it. The profile is shown below, so answer 'what do you know \
about me?' directly from it.
- The second-hand marketplaces searched for this user come from the \
secondhand_sites profile fact: a comma-separated domain list, e.g. \
'ebay.com,vinted.nl,marktplaats.nl' (max 8). When the user asks to add or \
remove a marketplace, save the FULL updated list with remember_fact under \
that key; forget_fact restores the default list. List changes take effect \
from the user's next message.
- Reply in plain text without markdown formatting. Keep replies compact - \
this is a chat.
- Layout: every option is its own block, separated by a blank line. First \
line names it (label, shop, price, key spec), then any short detail line, \
then that option's URL on a line of its own. NEVER collect links into a list \
at the end of the message - a link belongs to the option it describes, and a \
reader should be able to tap it while reading about it. Example:
Cheapest one-off
EUR 23.95 delivered - 5 L / 100 washes, parfum-bestel.nl (EUR 0.24 per wash)
https://www.parfum-bestel.nl/...

Best per unit
EUR 32.15 delivered - 3-pack, 28% less per wash
https://www.example.nl/...";

pub type LlmClient = openai::CompletionsClient;

/// Note on timeouts: rig's HTTP client has none, so a stalled MiniMax stream
/// would hang a request forever — which is how a user ended up staring at
/// "comparing 5 offers per gram" with nothing after it. Handing rig our own
/// configured client is not possible here (rig-core is on reqwest 0.12,
/// this crate on 0.13, so the `Client` types are unrelated), so the guard
/// lives in bot.rs instead, around the stream itself.
///
/// `base_url` is `MINIMAX_BASE_URL` in production and comes from
/// `Config::minimax_base_url`, which a self-hoster may point at a proxy of
/// their own and which the tests point at a closed port.
pub fn llm_client(api_key: &str, base_url: &str) -> Result<LlmClient> {
    Ok(openai::CompletionsClient::builder()
        .api_key(api_key)
        .base_url(base_url)
        .build()?)
}

/// Everything needed to assemble a per-request agent.
pub struct AgentDeps {
    pub llm: LlmClient,
    /// See `Config::flight_model`.
    pub flight_model: String,
    pub kagi: KagiClient,
    /// Headless-Chrome fallback for pages plain HTTP cannot read.
    pub renderer: Option<crate::tools::browser::Renderer>,
    /// Live bol.com catalogue when credentials are configured.
    pub bol: Option<crate::tools::bol::BolClient>,
    /// Second search engine when a key is configured; see WebSearchTool.
    pub perplexity: Option<crate::tools::perplexity::PerplexityClient>,
    pub http: reqwest::Client,
    pub ebay: Option<EbayClient>,
    /// Live flight search when a Duffel key is configured. Search only —
    /// Scout never creates an order, so no passenger details or payment
    /// ever pass through it.
    pub duffel: Option<crate::tools::duffel::DuffelClient>,
    /// Where Duffel Links returns the traveller — the bot's own Telegram
    /// address, read from `getMe` at startup. `None` disables booking
    /// links rather than sending people somewhere that is not ours.
    pub return_url: Option<String>,
    /// Whether Duffel has enabled Links on this account.
    pub links_enabled: bool,
    /// Second flights provider; see `tools::ignav`.
    pub ignav: Option<crate::tools::ignav::IgnavClient>,
    /// What each chat was last shown, so a booking lookup a turn later can
    /// tell a real offer id from an invented one. Shared across requests.
    pub shown: std::sync::Arc<crate::tools::shown::ShownFlights>,
    /// Conversations with a run in flight. See `run::begin_run`.
    pub running: std::sync::Arc<dashmap::DashSet<i64>>,
    /// The process-wide run slots; see `run::MAX_CONCURRENT_RUNS`.
    pub runs: std::sync::Arc<tokio::sync::Semaphore>,
    pub marktplaats: MarktplaatsClient,
    pub store: Store,
    pub secondhand_sites: Vec<String>,
}

/// One-shot classifier used when a chat resumes after the session TTL: does
/// the new message continue the previous conversation, or start a new
/// request? Errors should be treated as "new" by the caller (fresh session
/// is the safe default).
pub async fn continues_previous(
    llm: &LlmClient,
    previous_excerpt: &str,
    new_message: &str,
) -> Result<bool> {
    let agent = llm
        .agent(MODEL)
        .preamble(
            "You judge whether a new chat message continues the previous \
             conversation or starts an unrelated new request. Reply with \
             exactly one word: CONTINUE or NEW.",
        )
        .build();
    let question = format!(
        "Previous conversation (latest excerpts):\n{previous_excerpt}\n\n\
         New message:\n{new_message}\n\nCONTINUE or NEW?"
    );
    let verdict = crate::text::strip_thinking(&rig::completion::Prompt::prompt(&agent, question).await?);
    Ok(verdict.to_uppercase().contains("CONTINUE"))
}

/// How long one call to name a thread may take. The same 30s the outbound
/// HTTP client gives any other single call.
const TITLE_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// One-shot: a short name for a conversation, from its text. Tool-less,
/// like `continues_previous`, and for the same reason.
///
/// Bounded in time because nothing else bounds it: rig's client has no
/// timeout of its own (see [`llm_client`]), and this call sits under an HTTP
/// handler rather than under the run loop, whose `STREAM_STALL` guard is the
/// only other thing that would cut a hung connection loose.
pub async fn title_for(llm: &LlmClient, transcript: &str) -> Result<String> {
    let agent = llm
        .agent(MODEL)
        .preamble(
            "You name chat threads. Reply with a title of at most five words \
             in the language of the conversation, no quotes, no trailing \
             punctuation, nothing else.",
        )
        .build();
    let question = format!("Conversation:\n{transcript}\n\nTitle:");
    let answer = tokio::time::timeout(TITLE_BUDGET, rig::completion::Prompt::prompt(&agent, question))
        .await
        .map_err(|_| anyhow::anyhow!("the model did not answer in time"))??;
    Ok(crate::text::strip_thinking(&answer))
}

/// Cap on injected profile facts, bounding prompt growth.
const MAX_PROFILE_FACTS: usize = 50;

/// Profile fact holding an explicit search-language list.
pub const LANGUAGES_FACT_KEY: &str = "search_languages";
/// Profile fact listing shops worth a site-scoped query, each with the kind
/// of product it applies to: `123schoon.nl:cleaning, coolblue.nl:electronics`.
pub const SHOPS_FACT_KEY: &str = "favourite_shops";
/// Enough for the shops one household actually returns to.
const MAX_SHOPS: usize = 8;

/// Shops to search by name, as (domain, what it is for) pairs. A bare domain
/// with no category means every search.
///
/// Small shops do not rank: 123schoon.nl sells the Vanish powder a user
/// found through a shopping ad, and neither engine returns it for the
/// product's own name — but both return it for `site:123schoon.nl vanish
/// oxi action`. Scoping the query is the only thing that reaches them.
pub fn favourite_shops(facts: &[(String, String)]) -> Vec<(String, String)> {
    let Some((_, value)) = facts.iter().find(|(k, _)| k == SHOPS_FACT_KEY) else {
        return Vec::new();
    };
    let mut shops = Vec::new();
    for entry in value.split([',', ';', '\n']) {
        // Strip the scheme before splitting on ':', or "https://shop.nl:x"
        // splits into "https" and the rest.
        let entry = entry.trim().to_lowercase();
        let entry = entry
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_start_matches("www.");
        let (domain, category) = match entry.split_once(':') {
            Some((d, c)) => (d, c.trim().to_string()),
            None => (entry, String::new()),
        };
        let domain = domain
            .trim()
            .trim_end_matches('/')
            .split('/')
            .next()
            .unwrap_or_default()
            .to_string();
        if domain.contains('.') && !shops.iter().any(|(d, _): &(String, _)| *d == domain) {
            shops.push((domain, category));
            if shops.len() >= MAX_SHOPS {
                break;
            }
        }
    }
    shops
}
/// Languages per search, English included — one query each, run in parallel.
const MAX_LANGUAGES: usize = 3;

/// Country token -> language of that country's shops. Only the markets this
/// bot's users buy from; an unknown country simply means English-only.
const COUNTRY_LANGUAGES: &[(&str, &str)] = &[
    ("nl", "Dutch"),
    ("netherlands", "Dutch"),
    ("nederland", "Dutch"),
    ("holland", "Dutch"),
    ("be", "Dutch"),
    ("belgium", "Dutch"),
    ("de", "German"),
    ("germany", "German"),
    ("deutschland", "German"),
    ("at", "German"),
    ("fr", "French"),
    ("france", "French"),
    ("es", "Spanish"),
    ("spain", "Spanish"),
    ("it", "Italian"),
    ("italy", "Italian"),
    ("pl", "Polish"),
    ("poland", "Polish"),
];

/// Languages this user's product searches should cover, English first.
///
/// An explicit `search_languages` fact wins; otherwise the delivery country
/// decides, because local shops rank on local terms — searching "laundry
/// detergent" barely surfaces bol.com, "wasmiddel" does.
pub fn search_languages(facts: &[(String, String)]) -> Vec<String> {
    let mut langs = vec!["English".to_string()];
    let mut add = |lang: &str| {
        let lang = capitalize(lang);
        if !lang.is_empty() && !langs.contains(&lang) && langs.len() < MAX_LANGUAGES {
            langs.push(lang);
        }
    };

    if let Some((_, value)) = facts.iter().find(|(k, _)| k == LANGUAGES_FACT_KEY) {
        for lang in value.split([',', ';', '/']) {
            add(lang.trim());
        }
        return langs;
    }

    for (key, value) in facts.iter().filter(|(k, _)| k.starts_with("delivery_")) {
        for token in value.split(|c: char| !c.is_alphabetic()) {
            if let Some((_, lang)) = COUNTRY_LANGUAGES
                .iter()
                .find(|(c, _)| *c == token.to_lowercase())
            {
                add(lang);
            }
        }
        let _ = key;
    }
    langs
}

fn capitalize(s: &str) -> String {
    let mut chars = s.trim().chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars.flat_map(|c| c.to_lowercase())).collect(),
        None => String::new(),
    }
}

/// The system prompt plus the user's long-term profile. Injecting facts here
/// (instead of behind a recall tool) means the agent can never forget to
/// check them.
/// The conditional tools this agent actually gets, for the preamble.
///
/// Derived from the same `is_some()` checks that register them below, so a
/// rule cannot outlive its tool. Getting this wrong is not a cosmetic
/// problem: the model reads the preamble, calls what it describes, and the
/// whole request dies with `UnknownToolCall`.
fn available_tools(d: &AgentDeps) -> Vec<&'static str> {
    ALL_TOOLS
        .iter()
        .copied()
        .filter(|tool| match *tool {
            "search_bol" => d.bol.is_some(),
            "ask_flights" => d.duffel.is_some() || d.ignav.is_some(),
            // A name in ALL_TOOLS with no arm here is a rule that would
            // never be shown; `every_conditional_rule_is_wired_up` catches
            // the reverse, a rule with no name.
            _ => false,
        })
        .collect()
}

/// Every tool the preamble may describe, for callers that have them all.
pub const ALL_TOOLS: &[&str] = &["search_bol", "ask_flights"];

/// Drops the rules for tools this agent was not given.
///
/// A rule opening "When `<tool>` is available," is exactly and only about
/// that tool, so it goes when the tool does. Without this the preamble
/// advertises whatever the const happens to mention and the model calls it:
/// measured twice in production, `UnknownToolCall: search_flights` when the
/// tool was gated on Duffel alone, and `UnknownToolCall: search_bol` on an
/// install with no bol.com credentials, which cost a user their answer for
/// a glasses wipe kit.
///
/// The conditional phrasing was already there. It was addressed to the
/// model, which cannot check, rather than to the code, which can.
pub(crate) fn rules_for_available_tools(preamble: &str, available: &[&str]) -> String {
    preamble
        .split("\n- ")
        .enumerate()
        .filter(|(i, rule)| {
            // The head is the intro, not a rule.
            *i == 0
                || rule
                    .strip_prefix("When ")
                    .and_then(|r| r.split_once(" is available,"))
                    .is_none_or(|(tool, _)| available.contains(&tool))
        })
        .map(|(_, rule)| rule)
        .collect::<Vec<_>>()
        .join("\n- ")
}

pub fn preamble_with_profile(facts: &[(String, String)], available: &[&str]) -> String {
    let mut p = rules_for_available_tools(PREAMBLE, available);
    if !facts.is_empty() {
        p.push_str("\n\nKnown about this user (long-term profile):\n");
        for (key, value) in facts.iter().take(MAX_PROFILE_FACTS) {
            p.push_str(&format!("- {key}: {value}\n"));
        }
    }
    p.push_str(&format!(
        "\nSearch languages for this user: {}.\n",
        search_languages(facts).join(", ")
    ));
    let shops = favourite_shops(facts);
    if !shops.is_empty() {
        p.push_str("\nShops this user wants searched by name, and for what:\n");
        for (domain, category) in &shops {
            match category.is_empty() {
                true => p.push_str(&format!("- {domain}: any product\n")),
                false => p.push_str(&format!("- {domain}: {category}\n")),
            }
        }
    }
    // Last, because the date rule above points at the end of the prompt.
    p.push_str(&format!("\nToday's date is {} (UTC).\n", today()));
    p
}

/// Today, as every prompt is told it.
///
/// Until this existed no preamble, brief or prompt carried a date at all,
/// so a bare "23 September" was a guess and the guesses disagreed: one
/// production run sent 2025-09-25 to the flight providers eight times,
/// thirteen days before the September the traveller actually meant, and
/// got departure_date_in_past back for every one of them. Scout's reply
/// then had to ask which year they had meant, because it could not tell.
///
/// A date and not a timestamp on purpose: the preamble is part of the
/// prompt cache key on some providers, so this changes once a day rather
/// than once a request.
pub(crate) fn today() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// A rate as a percentage a person would say aloud: 0.03 -> "3%",
/// 0.035 -> "3.5%". Trailing zeros make it read like a spec, not a fee.
pub(crate) fn percentage(rate: f64) -> String {
    let pct = format!("{:.2}", rate * 100.0);
    let pct = pct.trim_end_matches('0').trim_end_matches('.');
    format!("{pct}%")
}

/// Note handed to the wrap-up agent when the turn budget runs out.
pub const WRAP_UP_NOTE: &str = "[system note] You have run out of research steps and cannot \
call any more tools. Answer now from what you already gathered above: give the options you \
did confirm, with their prices and links, and say plainly which parts you could not verify. \
A partial answer is what is wanted here - do not apologise and do not ask to continue.";

/// A tool-less agent over the same preamble and history, used when the turn
/// budget is exhausted. By that point the model usually has everything it
/// needs and only the final write-up is missing; without this the whole run
/// is thrown away and the user gets an apology instead of the prices we
/// already paid to look up.
pub fn wrap_up_agent(
    d: &AgentDeps,
    facts: &[(String, String)],
) -> rig::agent::Agent<openai::completion::CompletionModel> {
    d.llm
        .agent(MODEL)
        .preamble(&preamble_with_profile(facts, &available_tools(d)))
        .default_max_turns(1)
        .build()
}

/// The country whose currency fare prices should come back in.
///
/// Ignav defaults to US, which answers in dollars. Merged against Duffel's
/// euros the currency guard then drops every Ignav row, so the provider
/// contributes nothing at all — measured live before this existed. The
/// delivery country the user already told us is the right answer.
pub fn fare_market(facts: &[(String, String)]) -> Option<String> {
    let value = facts
        .iter()
        .find(|(k, _)| k.starts_with("delivery_"))
        .map(|(_, v)| v.trim().to_ascii_uppercase())?;
    // "NL" as given, and "Netherlands" resolved through the same table
    // that decides search languages.
    if value.len() == 2 && value.chars().all(|c| c.is_ascii_alphabetic()) {
        return Some(value);
    }
    let lower = value.to_lowercase();
    COUNTRY_LANGUAGES
        .iter()
        .find(|(name, _)| name.len() > 2 && lower.contains(*name))
        .and_then(|(name, _)| match *name {
            "netherlands" | "nederland" | "holland" => Some("NL".to_string()),
            "belgium" => Some("BE".to_string()),
            "germany" | "deutschland" => Some("DE".to_string()),
            "france" => Some("FR".to_string()),
            "spain" => Some("ES".to_string()),
            "italy" => Some("IT".to_string()),
            "poland" => Some("PL".to_string()),
            _ => None,
        })
}

/// The booking fee in force, or nothing when flights are not configured.
pub(crate) fn markup_rate(d: &AgentDeps) -> f64 {
    d.duffel.as_ref().map_or(0.0, |c| c.markup_rate())
}

/// Built per incoming message: tools capture the requesting account's
/// identity, so the LLM never sees or chooses ids. `events` carries the
/// nested flight desk's progress to the chat, and `pulse` its liveness to
/// the stall guard.
pub fn build_agent(
    d: &AgentDeps,
    run: &scout_api::RunContext,
    facts: &[(String, String)],
    events: scout_api::EventSink,
    pulse: std::sync::Arc<crate::run::Pulse>,
) -> rig::agent::Agent<openai::completion::CompletionModel> {
    let account_id = run.account_id;
    // One allowance per request, shared by both searching tools.
    let budget = std::sync::Arc::new(crate::tools::budget::SearchBudget::default());
    // One memo per request: a route asked for twice in one question is
    // answered from it rather than bought again.
    let flights = std::sync::Arc::new(crate::tools::budget::FlightBudget::default());
    let mut builder = d
        .llm
        .agent(MODEL)
        .preamble(&preamble_with_profile(facts, &available_tools(d)))
        .tool(WebSearchTool {
            kagi: d.kagi.clone(),
            perplexity: d.perplexity.clone(),
            budget: budget.clone(),
        })
        .tool(FetchPageTool::new(d.http.clone(), d.renderer.clone()))
        .tool(SecondhandSearchTool {
            client: d.kagi.clone(),
            http: d.http.clone(),
            ebay: d.ebay.clone(),
            marktplaats: d.marktplaats.clone(),
            sites: effective_sites(facts, &d.secondhand_sites),
            budget,
        })
        .tool(ComparePricesTool)
        .tool(RecordPurchaseTool { store: d.store.clone(), account_id })
        .tool(QueryPurchasesTool { store: d.store.clone(), account_id })
        .tool(ListRemindersTool { store: d.store.clone(), account_id })
        .tool(CancelReminderTool { store: d.store.clone(), account_id })
        .tool(RememberFactTool { store: d.store.clone(), account_id })
        .tool(ForgetFactTool { store: d.store.clone(), account_id });
    // Offered only when configured, so the model never sees a tool that
    // cannot work.
    // Offered only when the run has somewhere to deliver to. A reminder is
    // a promise to come back later, and a browser is not a channel anyone
    // polls — so on the web, for someone with no Telegram, the honest thing
    // is for the model never to see the tool rather than to accept a
    // reminder that would silently never arrive.
    if let Some(reply_to) = &run.reply_to {
        builder = builder.tool(CreateReminderTool {
            store: d.store.clone(),
            account_id,
            reply_to: reply_to.clone(),
        });
    }
    if let Some(bol) = &d.bol {
        builder = builder.tool(crate::tools::bol::BolSearchTool { client: bol.clone() });
    }
    // The flight desk, offered whenever at least one provider can answer
    // a flight question. Every flight-shaped tool lives inside it; the
    // main agent sees one tool and a report.
    if d.duffel.is_some() || d.ignav.is_some() {
        builder = builder.tool(crate::flights::ask_flights(d, run, facts, flights, events, pulse));
    }
    builder.default_max_turns(MAX_TURNS).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_conditional_rule_is_wired_up() {
        // The guard against a third UnknownToolCall. Adding a rule that
        // opens "When <tool> is available," without listing the tool in
        // ALL_TOOLS would leave it permanently visible — described to the
        // model whether or not the tool exists, which is the bug this all
        // came from.
        for rule in PREAMBLE.split("\n- ").skip(1) {
            if let Some((tool, _)) =
                rule.strip_prefix("When ").and_then(|r| r.split_once(" is available,"))
            {
                assert!(
                    ALL_TOOLS.contains(&tool),
                    "the preamble offers {tool:?} conditionally but nothing decides whether \
                     it is there; add it to ALL_TOOLS and to available_tools"
                );
            }
        }
    }

    #[test]
    fn the_preamble_never_describes_a_tool_this_agent_lacks() {
        // Measured twice in production: `UnknownToolCall: search_flights`
        // when the tool was gated on Duffel alone, and `UnknownToolCall:
        // search_bol` on an install with no bol.com credentials — that one
        // cost a user their answer for a glasses wipe kit. The preamble said
        // "When search_bol is available", which asks the model to check
        // something only the code can know.
        let without_bol: Vec<&str> =
            ALL_TOOLS.iter().copied().filter(|t| *t != "search_bol").collect();
        let p = preamble_with_profile(&[], &without_bol);
        assert!(!p.contains("search_bol"), "an absent tool is not mentioned at all");
        assert!(p.contains("ask_flights"), "the ones it does have stay");
        assert!(p.contains("compare_prices"), "and so do the unconditional rules");

        // With everything, nothing is lost.
        let all = preamble_with_profile(&[], ALL_TOOLS);
        assert!(all.contains("search_bol") && all.contains("ask_flights"));

        // The rule really is dropped whole, not just its first line.
        // The whole rule goes, not just the sentence naming the tool.
        assert!(!p.contains("Search it in Dutch"), "the rest of the rule went too: {p}");
        // But an unconditional rule that merely mentions the shop stays —
        // bol.com pages still arrive through ordinary web search.
        assert!(p.contains("the page text is not"), "the stock rule is untouched");

        // ask_flights owns more than one rule now, and every one of them
        // has to go with the tool: an install with no flight provider being
        // told to send "save this trip" to ask_flights is the same
        // UnknownToolCall by another name.
        let without_flights: Vec<&str> =
            ALL_TOOLS.iter().copied().filter(|t| *t != "ask_flights").collect();
        let f = preamble_with_profile(&[], &without_flights);
        assert!(!f.contains("ask_flights"), "an absent desk is not mentioned at all: {f}");
        assert!(!f.contains("keep the trip by name"), "the keep rule went with it: {f}");
    }

    #[test]
    fn the_main_prompt_carries_no_flight_rules_and_no_fee() {
        // The point of the split. The fee now arrives with the findings
        // (flights::guidance) and in search_flights' own notes.
        let p = preamble_with_profile(&[], ALL_TOOLS);
        assert!(!p.contains("Booking fee"), "got: {p}");
        for word in ["search_flights", "flex_days", "itinerary", "add_trip_segment", "price_status"] {
            assert!(!p.contains(word), "{word:?} belongs to the flight agent now");
        }
        assert!(p.contains("When ask_flights is available"), "got: {p}");
        assert!(p.contains("self-contained brief"), "got: {p}");
        // The brief must not be bought with an interrogation: passengers
        // default, and the desk names what is really missing.
        assert!(p.contains("one adult"), "got: {p}");
        // And the offer id alone is ambiguous between two booking tools.
        assert!(p.contains("its source"), "got: {p}");
        // Keeping a trip is the desk's errand too. Measured in production:
        // the traveller typed "Save this trip" the turn after the findings,
        // where guidance no longer reaches the parent, so it called
        // record_purchase and then claimed the trip was saved as purchase
        // id 11. The routing has to be durable, not carried by a report.
        assert!(p.contains("keep the trip by name"), "got: {p}");
        assert!(p.contains("record_purchase records something the traveller BOUGHT"), "got: {p}");
    }

    #[test]
    fn the_run_loop_hands_the_sink_to_the_agent_build() {
        // The specialist reports progress through the run's sink; without
        // it a flight question is a silent minute.
        let src = include_str!("run.rs");
        let src = &src[..src.find("#[cfg(test)]").expect("the tests must come last")];
        let call = &src[src.find("build_agent(").expect("the agent build must exist")..];
        // Up to the parenthesis matching the call's own, not the first one
        // closing an argument's `.clone()`.
        let mut depth = 0usize;
        let end = call
            .char_indices()
            .find_map(|(i, c)| match c {
                '(' => {
                    depth += 1;
                    None
                }
                ')' => {
                    depth -= 1;
                    (depth == 0).then_some(i)
                }
                _ => None,
            })
            .expect("the call must close");
        let call = &call[..end];
        assert!(call.contains("events.clone()"), "the sink must reach build_agent: {call}");
        assert!(call.contains("pulse.clone()"), "the pulse must reach build_agent: {call}");
    }

    #[test]
    fn the_fare_market_comes_from_the_delivery_country() {
        // Without this Ignav answers in USD, rank() drops every row for
        // being in another currency, and the second provider silently does
        // nothing at all.
        assert_eq!(fare_market(&facts(&[("delivery_country", "NL")])).as_deref(), Some("NL"));
        assert_eq!(fare_market(&facts(&[("delivery_country", "nl")])).as_deref(), Some("NL"));
        assert_eq!(
            fare_market(&facts(&[("delivery_country", "Netherlands")])).as_deref(),
            Some("NL")
        );
        assert_eq!(fare_market(&facts(&[("delivery_country", "Germany")])).as_deref(), Some("DE"));
        // Nothing known: leave the client on its own default rather than
        // guessing a country for someone.
        assert_eq!(fare_market(&[]), None);
        assert_eq!(fare_market(&facts(&[("shoe_size", "44")])), None);
    }

    #[test]
    fn profile_is_appended_when_present() {
        let plain = preamble_with_profile(&[], ALL_TOOLS);
        assert!(plain.starts_with(PREAMBLE));
        // with nothing known, the only search language is English
        assert!(plain.contains("Search languages for this user: English."));
        assert!(!plain.contains("long-term profile"));

        let facts = vec![
            ("delivery_country".to_string(), "NL".to_string()),
            ("shoe_size".to_string(), "44".to_string()),
        ];
        let with = preamble_with_profile(&facts, ALL_TOOLS);
        assert!(with.starts_with(PREAMBLE));
        assert!(with.contains("- delivery_country: NL"));
        assert!(with.contains("- shoe_size: 44"));
    }

    fn facts(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn search_languages_come_from_the_delivery_country() {
        assert_eq!(
            search_languages(&facts(&[("delivery_country", "NL")])),
            vec!["English", "Dutch"]
        );
        // the other user's profile stores a city, not a country
        assert_eq!(
            search_languages(&facts(&[("delivery_city", "Hilversum, NL")])),
            vec!["English", "Dutch"]
        );
        assert_eq!(
            search_languages(&facts(&[("delivery_country", "Germany")])),
            vec!["English", "German"]
        );
        // nothing known, or a country we have no mapping for: English only
        assert_eq!(search_languages(&[]), vec!["English"]);
        assert_eq!(
            search_languages(&facts(&[("delivery_country", "JP")])),
            vec!["English"]
        );
    }

    #[test]
    fn explicit_search_languages_fact_wins_and_is_capped() {
        assert_eq!(
            search_languages(&facts(&[
                ("delivery_country", "NL"),
                ("search_languages", "dutch, GERMAN"),
            ])),
            vec!["English", "Dutch", "German"]
        );
        // English is always there, duplicates and overflow are dropped
        assert_eq!(
            search_languages(&facts(&[("search_languages", "english, dutch, german, french")])),
            vec!["English", "Dutch", "German"]
        );
    }

    #[test]
    fn favourite_shops_are_parsed_with_their_category() {
        assert_eq!(
            favourite_shops(&facts(&[(
                "favourite_shops",
                "https://www.123schoon.nl/:Cleaning Products, coolblue.nl:electronics"
            )])),
            vec![
                ("123schoon.nl".to_string(), "cleaning products".to_string()),
                ("coolblue.nl".to_string(), "electronics".to_string()),
            ]
        );
        // A bare domain applies everywhere.
        assert_eq!(
            favourite_shops(&facts(&[("favourite_shops", "bol.com")])),
            vec![("bol.com".to_string(), String::new())]
        );
        // Junk entries are dropped, duplicates collapse.
        assert_eq!(
            favourite_shops(&facts(&[("favourite_shops", "notadomain, bol.com:books, bol.com:toys")])),
            vec![("bol.com".to_string(), "books".to_string())]
        );
        assert!(favourite_shops(&[]).is_empty());
    }

    #[test]
    fn the_profile_block_pairs_each_shop_with_its_kind_of_product() {
        let p = preamble_with_profile(&facts(&[(
            "favourite_shops",
            "123schoon.nl:cleaning products, bol.com",
        )]), ALL_TOOLS);
        assert!(p.contains("- 123schoon.nl: cleaning products"), "got: {p}");
        assert!(p.contains("- bol.com: any product"), "got: {p}");

        // Nothing listed, nothing said: the rule must not invite a site:
        // query at a shop the user never named.
        let none = preamble_with_profile(&[], ALL_TOOLS);
        assert!(!none.contains("Shops this user wants searched"));
    }

    #[test]
    fn the_preamble_states_today_and_says_a_bare_date_means_the_next_one() {
        // Until this line existed, "23 September" was whatever year the
        // model felt like: one run sent 2025-09-25 to the providers eight
        // times, thirteen days before the September that was meant.
        // Expected the way the code works it out, never written down - a
        // date typed into a test passes today and fails tomorrow.
        let p = preamble_with_profile(&[], ALL_TOOLS);
        let date = p
            .lines()
            .find_map(|l| l.strip_prefix("Today's date is ")?.strip_suffix(" (UTC)."))
            .unwrap_or_else(|| panic!("the preamble must state today's date: {p}"));
        assert_eq!(date, chrono::Utc::now().format("%Y-%m-%d").to_string(), "got: {date}");
        // And in a shape nobody has to guess at: 2026-09-10 says which
        // number is the month, 10/09 does not.
        assert!(
            chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok(),
            "the date must be YYYY-MM-DD: {date}"
        );

        // Knowing the day is not the rule; a model will still write a past
        // year out of habit unless told what a bare date means.
        assert!(p.contains("means its NEXT occurrence"), "got: {p}");
        assert!(p.contains("Write the year out in full"), "got: {p}");
        // The rule points at the end of the prompt, so the date has to be
        // there even for a user with shops and facts appended after it.
        let full = preamble_with_profile(
            &facts(&[("delivery_country", "NL"), ("favourite_shops", "bol.com")]),
            ALL_TOOLS,
        );
        assert!(full.trim_end().ends_with(&format!("Today's date is {date} (UTC).")), "got: {full}");
    }

    #[test]
    fn profile_block_states_the_search_languages() {
        let p = preamble_with_profile(&facts(&[("delivery_country", "NL")]), ALL_TOOLS);
        assert!(p.contains("Search languages for this user: English, Dutch."), "got: {p}");
    }

    #[test]
    fn the_call_that_names_a_thread_is_bounded_in_time() {
        // No model is reachable in a test, so the budget is asserted from
        // the source. `title_for` runs under an HTTP handler, outside the
        // run loop's stall guard, and rig's client has no timeout — without
        // this the request hangs as long as the connection does. Bounded to
        // the function's own body, so a `timeout(TITLE_BUDGET` anywhere
        // else in the file cannot stand in for it.
        let src = include_str!("agent.rs");
        let start = src.find("pub async fn title_for").expect("title_for must exist");
        let end = src[start..].find("\n}").expect("title_for must end") + start;
        let body = &src[start..end];
        assert!(body.contains("timeout(TITLE_BUDGET"), "the title call must carry its own budget");
    }
}
