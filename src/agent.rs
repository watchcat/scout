use crate::store::Store;
use crate::tools::ebay::EbayClient;
use crate::tools::fetch::FetchPageTool;
use crate::tools::kagi::{KagiClient, KagiSearchTool};
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
/// fetch_page opens -> answer) legitimately needs ~10; 12 leaves headroom
/// while still bounding a runaway loop.
pub const MAX_TURNS: usize = 12;
/// Conversation history cap per chat (messages, not exchanges).
pub const HISTORY_CAP: usize = 20;

pub const PREAMBLE: &str = "\
You are Scout, a product-research assistant living in a Telegram chat. You help \
the user find products online, compare options, and remember their purchases. \
You never buy anything yourself.

Rules:
- When the user asks to find or buy something, ALWAYS call query_purchases first \
to check whether they bought it (or something similar) before. Mention relevant \
history in your reply, including cadence you notice (e.g. 'you buy this roughly \
monthly, last on 2026-06-28 from Amazon').
- If you notice a periodic purchase with no reminder, offer to set one up. Call \
create_reminder ONLY after the user explicitly agrees.
- When the user mentions having bought something, record it with record_purchase \
and confirm what you saved.
- Use kagi_search for general product searches. Use search_secondhand when the \
user wants used items or second-hand is a sensible option (electronics, \
furniture, bikes, tools...).
- Search results often include retailer search/listing pages (URLs containing \
/s/?, /search, ?q=, ?searchtext=). NEVER present those as a product link. Open \
a promising listing or product page with fetch_page and take the direct \
product URL and price from it. Queries that include brand plus model number \
surface direct product pages more often.
- Budget your steps: open at most 3 pages with fetch_page per request, \
picking the most promising candidates. Prefer answering with what you have \
over exhaustively verifying everything.
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
- Reply in plain text without markdown formatting. Put URLs on their own lines. \
Keep replies compact - this is a chat.";

pub type LlmClient = openai::CompletionsClient;

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
    p
}

/// Built per incoming message: tools capture the requesting user's identity,
/// so the LLM never sees or chooses user ids.
pub fn build_agent(
    d: &AgentDeps,
    user_id: i64,
    chat_id: i64,
    facts: &[(String, String)],
) -> rig::agent::Agent<openai::completion::CompletionModel> {
    d.llm
        .agent(MODEL)
        .preamble(&preamble_with_profile(facts))
        .tool(KagiSearchTool(d.kagi.clone()))
        .tool(FetchPageTool { http: d.http.clone() })
        .tool(SecondhandSearchTool {
            client: d.kagi.clone(),
            http: d.http.clone(),
            ebay: d.ebay.clone(),
            marktplaats: d.marktplaats.clone(),
            sites: effective_sites(facts, &d.secondhand_sites),
        })
        .tool(ComparePricesTool)
        .tool(RecordPurchaseTool { store: d.store.clone(), user_id })
        .tool(QueryPurchasesTool { store: d.store.clone(), user_id })
        .tool(CreateReminderTool { store: d.store.clone(), user_id, chat_id })
        .tool(ListRemindersTool { store: d.store.clone(), user_id })
        .tool(CancelReminderTool { store: d.store.clone(), user_id })
        .tool(RememberFactTool { store: d.store.clone(), user_id })
        .tool(ForgetFactTool { store: d.store.clone(), user_id })
        .default_max_turns(MAX_TURNS)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_is_appended_when_present() {
        let plain = preamble_with_profile(&[]);
        assert_eq!(plain, PREAMBLE);

        let facts = vec![
            ("delivery_country".to_string(), "NL".to_string()),
            ("shoe_size".to_string(), "44".to_string()),
        ];
        let with = preamble_with_profile(&facts);
        assert!(with.starts_with(PREAMBLE));
        assert!(with.contains("- delivery_country: NL"));
        assert!(with.contains("- shoe_size: 44"));
    }
}
