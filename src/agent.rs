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
/// The full flow (query_purchases -> search -> secondhand -> a few
/// fetch_page opens -> compare_prices -> answer) legitimately needs ~12 now
/// that price comparisons are mandatory; 16 leaves headroom while still
/// bounding a runaway loop. Running out is no longer fatal — see
/// [`wrap_up_agent`].
pub const MAX_TURNS: usize = 16;
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
search, open at most 3 pages, compare, answer.
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
- Budget your steps: open at most 3 pages with fetch_page per request, \
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
pub fn preamble_with_profile(facts: &[(String, String)]) -> String {
    let mut p = PREAMBLE.to_string();
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
    p
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
        .preamble(&preamble_with_profile(facts))
        .default_max_turns(1)
        .build()
}

/// Built per incoming message: tools capture the requesting user's identity,
/// so the LLM never sees or chooses user ids.
pub fn build_agent(
    d: &AgentDeps,
    user_id: i64,
    chat_id: i64,
    facts: &[(String, String)],
) -> rig::agent::Agent<openai::completion::CompletionModel> {
    // One allowance per request, shared by both searching tools.
    let budget = std::sync::Arc::new(crate::tools::budget::SearchBudget::default());
    let mut builder = d
        .llm
        .agent(MODEL)
        .preamble(&preamble_with_profile(facts))
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
        .tool(RecordPurchaseTool { store: d.store.clone(), user_id })
        .tool(QueryPurchasesTool { store: d.store.clone(), user_id })
        .tool(CreateReminderTool { store: d.store.clone(), user_id, chat_id })
        .tool(ListRemindersTool { store: d.store.clone(), user_id })
        .tool(CancelReminderTool { store: d.store.clone(), user_id })
        .tool(RememberFactTool { store: d.store.clone(), user_id })
        .tool(ForgetFactTool { store: d.store.clone(), user_id });
    // Offered only when configured, so the model never sees a tool that
    // cannot work.
    if let Some(bol) = &d.bol {
        builder = builder.tool(crate::tools::bol::BolSearchTool { client: bol.clone() });
    }
    builder.default_max_turns(MAX_TURNS).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_is_appended_when_present() {
        let plain = preamble_with_profile(&[]);
        assert!(plain.starts_with(PREAMBLE));
        // with nothing known, the only search language is English
        assert!(plain.contains("Search languages for this user: English."));
        assert!(!plain.contains("long-term profile"));

        let facts = vec![
            ("delivery_country".to_string(), "NL".to_string()),
            ("shoe_size".to_string(), "44".to_string()),
        ];
        let with = preamble_with_profile(&facts);
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
        )]));
        assert!(p.contains("- 123schoon.nl: cleaning products"), "got: {p}");
        assert!(p.contains("- bol.com: any product"), "got: {p}");

        // Nothing listed, nothing said: the rule must not invite a site:
        // query at a shop the user never named.
        let none = preamble_with_profile(&[]);
        assert!(!none.contains("Shops this user wants searched"));
    }

    #[test]
    fn profile_block_states_the_search_languages() {
        let p = preamble_with_profile(&facts(&[("delivery_country", "NL")]));
        assert!(p.contains("Search languages for this user: English, Dutch."), "got: {p}");
    }
}
