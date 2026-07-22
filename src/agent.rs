use crate::store::Store;
use crate::tools::fetch::FetchPageTool;
use crate::tools::kagi::{KagiClient, KagiSearchTool};
use crate::tools::purchases::{QueryPurchasesTool, RecordPurchaseTool};
use crate::tools::reminders::{CancelReminderTool, CreateReminderTool, ListRemindersTool};
use crate::tools::secondhand::SecondhandSearchTool;
use anyhow::Result;
use rig::client::CompletionClient;
use rig::providers::openai;

pub const MINIMAX_BASE_URL: &str = "https://api.minimax.io/v1";
pub const MODEL: &str = "minimax-m3";
/// Cap on model calls per request so a confused agent can't burn credits. 8
/// leaves headroom for the mandated query_purchases -> search -> secondhand
/// -> summarize -> answer flow.
pub const MAX_TURNS: usize = 8;
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
- Always include the price (with currency) and a direct link for every option \
you present. At most 5 options, best first. If you genuinely could not reach a \
direct product page, say so explicitly rather than passing off a listing URL.
- If key criteria are missing (budget, country for shipping, size, must-have \
features), ask before searching.
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
    pub store: Store,
    pub secondhand_sites: Vec<String>,
}

/// Built per incoming message: tools capture the requesting user's identity,
/// so the LLM never sees or chooses user ids.
pub fn build_agent(
    d: &AgentDeps,
    user_id: i64,
    chat_id: i64,
) -> rig::agent::Agent<openai::completion::CompletionModel> {
    d.llm
        .agent(MODEL)
        .preamble(PREAMBLE)
        .tool(KagiSearchTool(d.kagi.clone()))
        .tool(FetchPageTool { http: d.http.clone() })
        .tool(SecondhandSearchTool {
            client: d.kagi.clone(),
            sites: d.secondhand_sites.clone(),
        })
        .tool(RecordPurchaseTool { store: d.store.clone(), user_id })
        .tool(QueryPurchasesTool { store: d.store.clone(), user_id })
        .tool(CreateReminderTool { store: d.store.clone(), user_id, chat_id })
        .tool(ListRemindersTool { store: d.store.clone(), user_id })
        .tool(CancelReminderTool { store: d.store.clone(), user_id })
        .default_max_turns(MAX_TURNS)
        .build()
}
