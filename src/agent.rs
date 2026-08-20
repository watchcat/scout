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
use crate::tools::trips::{
    AddTripOptionTool, AddTripSegmentTool, ChooseTripOptionTool, DeleteTripTool,
    DropTripSegmentTool, ShowTripTool, UpdateTripSegmentTool,
};
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
You are Scout, a product-research assistant living in a Telegram chat. You help \
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
- When search_flights is available, use it for every flight question - never \
a web search, and never compare_prices, whose per-unit arithmetic means \
nothing for a flight. It asks the airlines directly, so its prices, times \
and flight numbers are current. Work out the airport codes yourself rather \
than asking: Amsterdam is AMS, Lisbon LIS, London LHR (or LON for all its \
airports), and dates go in as YYYY-MM-DD. Take every number from its output \
verbatim - cheapest and fastest are ranked in Rust and are not yours to \
recompute - and quote its route field so the reply cannot drift onto a \
route nobody searched. Present the picks under their own headings - Cheapest, Fastest, Best \
balance - one to two options each, in that order, and offer every one you \
are given. They are already chosen from hundreds and no option appears \
under two headings, so anything you drop is a choice removed for no reason. \
Best balance is the option closest to being both the cheapest and the \
quickest, worked out in Rust; do not recompute it or argue with it. When a \
group is empty, leave the heading out rather than explaining its absence, \
and when one option is both cheapest and quickest say so - it is the best \
fact in the answer. Options can come from two providers and \
every one carries price_status, which changes how you are allowed to quote \
it. 'bookable' is a live offer someone can pay right now: quote it as a \
price. 'approximate' and 'unconfirmed' are fares seen elsewhere that still \
have to be checked on the seller's page: quote those as 'from EUR 180' and \
say where they came from - NEVER present one as a plain price and never as \
'the cheapest' without saying it is not bookable. When self_transfer is \
true the trip is two separate tickets: the traveller re-checks their bags \
and carries the risk if the first flight is late, so say that every time it \
appears, however cheap it looks. departing_at_local and arriving_at_local are each in \
the local time of their own airport with no offset, so NEVER subtract them \
to work out how long a flight takes: LHR 10:03 to JFK 13:01 is a 7h58m \
flight, not a 2h58m one. Give journey length from the duration field, which \
is already written out for you. For any flight with a change of plane, name \
the connection airport and the wait from the connections list - 'changes at \
PVG, 3h 20m' - because that is what makes one itinerary better than another \
at the same price; never work a layover out from flight numbers or from your \
own knowledge of an airline's network. When changes_airport is true, say so \
loudly: the traveller lands at one airport and departs from another, with \
their own bags and no protection if the first flight is late. A connection \
whose layover is null means the offer did not state the times - say that \
rather than guessing, and do not offer to go and look it up elsewhere. \
Every leg carries an itinerary line already drawn for you - 'AMS 20:15 \
15.09 ✈ PVG 3h 20m ✈ HKG 20:35 16.09'. Put it on its own line under the \
option it belongs to, copied EXACTLY as given: do not retype the times, \
reorder it, translate it, add or remove the plane symbols, or rebuild it \
from the other fields. A return trip has one such line per leg, outbound \
first. It replaces listing the times in prose - say the price, the airline \
and what matters about the option, then the line. Prices are the whole trip for all passengers. Say \
what its notes say when they matter: a cheapest option that takes hours \
longer, bags that are not included, offers left out for being in another \
currency. found: 0 means nothing flies that route that day - say so plainly \
rather than apologising or guessing at alternatives. When the traveller says their dates are flexible, or asks whether \
another day is cheaper, pass flex_days (max 3) instead of searching each \
date yourself: it prices the whole window in one call and returns by_date, \
the cheapest fare per day. Present that as a short list, cheapest day \
marked, and say which days were covered - the route field ends in ±N. Do \
NOT ask for flex_days on an ordinary search: each day in the window is a \
separate paid search. Nobody can price a whole month; if that is what is \
wanted, say a week either side is the most that can be checked and pick the \
part that matters. A flight price expires \
within minutes, so never repeat one from earlier in the conversation; search \
again. When the user picks an ignav row to book, call \
flight_booking_links with that row's offer_id. It returns the airline's own \
page and any resellers, each with a price, and they open with the flight \
already selected - so unlike the Duffel link, there is nothing for the user \
to re-enter. Give the airline's link first and name any reseller that is \
cheaper, with both prices, rather than choosing for them. The fare is \
re-checked at that moment: if a note says it has risen or fallen, tell them \
the new price before they open anything and never repeat the old one. \
When create_booking_link is available and the user says they want to \
book a Duffel row, call it and give them the link on its own line. Say plainly what it \
is: Duffel's own checkout, where they pick the flight and pay; Scout never \
sees passenger or card details. It CANNOT be pre-filled - it opens on its \
own search box - so repeat the route, date and price for them to enter, and \
say the link is single-use and short-lived. Never re-send an old one; ask \
for a fresh link each time. Without that tool, Scout cannot book at all: \
give the numbers and let the user buy from the airline.
- When someone is planning more than one flight — a multi-city route, or a trip \
they are assembling over several messages — build it with the trip tools \
rather than holding it in your head. Write it down as you go, in the same \
reply you propose it in: call add_trip_segment for each leg the moment you \
have a route and a date, and add_trip_option for each flight you found, \
before you ask them to confirm anything. Do not wait for approval to record \
a plan — approval is what finalise_trip is for, and a plan you are still \
holding in your head is one that is gone by the next message. When they \
change a date or a leg afterwards, call update_trip_segment on that one \
segment. NEVER delete the trip and build it again, and never drop and re-add \
a segment to change it: both throw away every option parked on every other \
segment, and dropping renumbers everything after it so your next call lands \
on the wrong leg. Never describe a trip from memory. Every trip tool hands back the whole \
trip, including not_ready: when that is present the trip cannot be priced \
and the reason says which segment is missing what, and when it is absent \
the trip is complete. Say what it says. If a call failed, the trip did NOT \
change - do not report a leg as parked because you meant to park it, and do \
not summarise a trip you have not seen this turn; call show_trip and read it \
back. When you make several edits in one turn, trust each call's own \
'changed' line over the trip snapshot beside it: a snapshot is from the \
moment that call ran, so an earlier one does not show a later edit and that \
is not a failure. Believe the last snapshot, or call show_trip once at the \
end. A segment is one direction on one date, \
so a return is two segments. If they are undecided between flights, park each \
with add_trip_option and decided=false rather than picking for them; several \
options may sit on one segment and cost nothing extra. Quote a trip's prices \
as of when each option was parked, never as a current total: they are stale \
by construction and only finalising re-prices them. Finalising is the only \
thing that produces current prices, and it costs a search per segment, so \
call it when the trip is settled rather than to check on it. Present both \
totals it returns and never drop the note about separate tickets: a link \
per segment is a ticket per segment, and the traveller is carrying the risk \
at every join. If the single-ticket total is missing, say that it is \
missing — it is not evidence that separate booking is better.
- If a booking fee is listed below, every flight price you are given \
ALREADY includes it, and it is what the checkout will charge - quote the \
numbers unchanged. Say once, in plain words, that prices include that \
booking fee. Never hide it, never add it on yourself, and never quote a \
price without it.
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
you present. At most 5 options, best first - this is about products; flights \
have no product page to link and their count is set by the rows the flight \
search returns. If you genuinely could not reach a \
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
pub fn llm_client(api_key: &str) -> Result<LlmClient> {
    Ok(openai::CompletionsClient::builder()
        .api_key(api_key)
        .base_url(MINIMAX_BASE_URL)
        .build()?)
}

/// Everything needed to assemble a per-request agent.
pub struct AgentDeps {
    pub llm: LlmClient,
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
            "search_flights" | "finalise_trip" => d.duffel.is_some() || d.ignav.is_some(),
            // A name in ALL_TOOLS with no arm here is a rule that would
            // never be shown; `every_conditional_rule_is_wired_up` catches
            // the reverse, a rule with no name.
            _ => false,
        })
        .collect()
}

/// Every tool the preamble may describe, for callers that have them all.
pub const ALL_TOOLS: &[&str] = &["search_bol", "search_flights", "finalise_trip"];

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
fn rules_for_available_tools(preamble: &str, available: &[&str]) -> String {
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

pub fn preamble_with_profile(
    facts: &[(String, String)],
    markup_rate: f64,
    available: &[&str],
) -> String {
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
    // Named here because the flight rules above tell the model that quoted
    // prices already include it; without the number that instruction has
    // nothing to point at.
    if markup_rate > 0.0 {
        p.push_str(&format!(
            "\nBooking fee: flight prices you are shown already include a \
             {} booking fee, which is what the checkout charges.\n",
            percentage(markup_rate)
        ));
    }
    p
}

/// A rate as a percentage a person would say aloud: 0.03 -> "3%",
/// 0.035 -> "3.5%". Trailing zeros make it read like a spec, not a fee.
fn percentage(rate: f64) -> String {
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
        .preamble(&preamble_with_profile(facts, markup_rate(d), &available_tools(d)))
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
fn markup_rate(d: &AgentDeps) -> f64 {
    d.duffel.as_ref().map_or(0.0, |c| c.markup_rate())
}

/// Built per incoming message: tools capture the requesting user's identity,
/// so the LLM never sees or chooses user ids.
pub fn build_agent(
    d: &AgentDeps,
    account_id: i64,
    chat_id: i64,
    facts: &[(String, String)],
) -> rig::agent::Agent<openai::completion::CompletionModel> {
    // One allowance per request, shared by both searching tools.
    let budget = std::sync::Arc::new(crate::tools::budget::SearchBudget::default());
    // One memo per request: a route asked for twice in one question is
    // answered from it rather than bought again.
    let flights = std::sync::Arc::new(crate::tools::budget::FlightBudget::default());
    let mut builder = d
        .llm
        .agent(MODEL)
        .preamble(&preamble_with_profile(facts, markup_rate(d), &available_tools(d)))
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
        .tool(CreateReminderTool { store: d.store.clone(), account_id, chat_id })
        .tool(ListRemindersTool { store: d.store.clone(), account_id })
        .tool(CancelReminderTool { store: d.store.clone(), account_id })
        .tool(RememberFactTool { store: d.store.clone(), account_id })
        .tool(ForgetFactTool { store: d.store.clone(), account_id })
        // Trip planning. Registered unconditionally: a trip is a plan, and
        // planning one needs no provider at all. Only finalising does.
        .tool(AddTripSegmentTool { store: d.store.clone(), account_id })
        .tool(AddTripOptionTool {
            store: d.store.clone(),
            account_id,
            shown: d.shown.clone(),
            chat_id,
        })
        .tool(ChooseTripOptionTool { store: d.store.clone(), account_id })
        .tool(ShowTripTool { store: d.store.clone(), account_id })
        .tool(UpdateTripSegmentTool { store: d.store.clone(), account_id })
        .tool(DropTripSegmentTool { store: d.store.clone(), account_id })
        .tool(DeleteTripTool { store: d.store.clone(), account_id });
    // Offered only when configured, so the model never sees a tool that
    // cannot work.
    if let Some(bol) = &d.bol {
        builder = builder.tool(crate::tools::bol::BolSearchTool { client: bol.clone() });
    }
    // Either provider can answer a flight question, so the tool is offered
    // whenever at least one is configured. Gating it on Duffel alone left
    // the model calling a tool that was not there.
    if d.duffel.is_some() || d.ignav.is_some() {
        builder = builder.tool(crate::tools::duffel::FlightSearchTool {
            duffel: d.duffel.clone(),
            store: d.store.clone(),
            account_id,
            // One allowance and one memo per user request, like the search
            // budget above. Cloned, not moved: finalise_trip below shares
            // this same allowance, so a request that searches and then
            // finalises draws on one cap rather than two.
            budget: flights.clone(),
            // Outlives the request: booking happens a turn later, when the
            // memo above is gone.
            shown: d.shown.clone(),
            chat_id,
            // Priced in the traveller's own currency, or Duffel's euros
            // and Ignav's dollars never get compared.
            ignav: d
                .ignav
                .clone()
                .map(|c| match fare_market(facts) {
                    Some(market) => c.with_market(&market),
                    None => c,
                }),
        });
        // Where an Ignav row can actually be bought. Unlike Duffel's
        // hosted checkout these open with the flight already selected.
        if let Some(ignav) = &d.ignav {
            builder = builder.tool(crate::tools::ignav::BookingLinksTool {
                // No market here: an ignav_id lookup rejects one, because
                // the id already carries the market of the search that
                // produced it.
                client: ignav.clone(),
                shown: d.shown.clone(),
                chat_id,
            });
        }
        builder = builder.tool(crate::tools::trips::FinaliseTripTool {
            store: d.store.clone(),
            account_id,
            duffel: d.duffel.clone(),
            // Same wrapper as FlightSearchTool above, and for the same
            // reason: finalising re-prices every segment through
            // IgnavClient::search, which reads self.market, so an
            // unwrapped client would silently re-price a Dutch traveller's
            // trip in Ignav's default US market. Safe to reuse for booking
            // links too — IgnavClient::booking_links hardcodes its request
            // to {"ignav_id": ...} and never reads the market, since the id
            // already carries it.
            ignav: d
                .ignav
                .clone()
                .map(|c| match fare_market(facts) {
                    Some(market) => c.with_market(&market),
                    None => c,
                }),
            // The same allowance the search tool got: one request, one cap.
            budget: flights.clone(),
        });
    }
    // Duffel's hosted checkout: needs Duffel itself, Links enabled on the
    // account, and somewhere to send people back to. Registering it
    // without all three means the model promises a booking it cannot make.
    if let (Some(duffel), Some(return_url)) = (
        &d.duffel,
        d.return_url.as_ref().filter(|_| d.links_enabled),
    ) {
        builder = builder.tool(crate::tools::duffel::BookingLinkTool {
            client: duffel.clone(),
            account_id,
            return_url: return_url.clone(),
        });
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
        let p = preamble_with_profile(&[], 0.0, &without_bol);
        assert!(!p.contains("search_bol"), "an absent tool is not mentioned at all");
        assert!(p.contains("search_flights"), "the ones it does have stay");
        assert!(p.contains("compare_prices"), "and so do the unconditional rules");

        // With everything, nothing is lost.
        let all = preamble_with_profile(&[], 0.0, ALL_TOOLS);
        assert!(all.contains("search_bol") && all.contains("search_flights"));

        // The rule really is dropped whole, not just its first line.
        // The whole rule goes, not just the sentence naming the tool.
        assert!(!p.contains("Search it in Dutch"), "the rest of the rule went too: {p}");
        // But an unconditional rule that merely mentions the shop stays —
        // bol.com pages still arrive through ordinary web search.
        assert!(p.contains("the page text is not"), "the stock rule is untouched");
    }

    #[test]
    fn the_booking_fee_is_stated_in_the_preamble_when_one_is_charged() {
        // The rule above tells the model prices "already include it", which
        // is only actionable if the preamble says what it is.
        let free = preamble_with_profile(&[], 0.0, ALL_TOOLS);
        assert!(!free.contains("Booking fee"), "no fee, no line");

        let charged = preamble_with_profile(&[], 0.03, ALL_TOOLS);
        assert!(charged.contains("Booking fee"), "got: {charged}");
        assert!(charged.contains("3%"), "stated as a percentage, got: {charged}");
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
        let plain = preamble_with_profile(&[], 0.0, ALL_TOOLS);
        assert!(plain.starts_with(PREAMBLE));
        // with nothing known, the only search language is English
        assert!(plain.contains("Search languages for this user: English."));
        assert!(!plain.contains("long-term profile"));

        let facts = vec![
            ("delivery_country".to_string(), "NL".to_string()),
            ("shoe_size".to_string(), "44".to_string()),
        ];
        let with = preamble_with_profile(&facts, 0.0, ALL_TOOLS);
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
        )]), 0.0, ALL_TOOLS);
        assert!(p.contains("- 123schoon.nl: cleaning products"), "got: {p}");
        assert!(p.contains("- bol.com: any product"), "got: {p}");

        // Nothing listed, nothing said: the rule must not invite a site:
        // query at a shop the user never named.
        let none = preamble_with_profile(&[], 0.0, ALL_TOOLS);
        assert!(!none.contains("Shops this user wants searched"));
    }

    #[test]
    fn profile_block_states_the_search_languages() {
        let p = preamble_with_profile(&facts(&[("delivery_country", "NL")]), 0.0, ALL_TOOLS);
        assert!(p.contains("Search languages for this user: English, Dutch."), "got: {p}");
    }
}
