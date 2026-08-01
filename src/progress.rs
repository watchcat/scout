//! Live feedback while the agent works.
//!
//! A product search runs a dozen tool calls before it writes a word, so the
//! chat sits silent for the better part of a minute. One message is sent up
//! front and edited as work happens — first with what the agent is doing,
//! then with its answer as it streams in.

use std::time::{Duration, Instant};
use teloxide::prelude::*;
use teloxide::types::MessageId;

/// Telegram rate-limits edits; this is comfortably under what it tolerates
/// while still feeling live.
const MIN_EDIT_INTERVAL: Duration = Duration::from_millis(1200);

/// One Telegram message, repeatedly rewritten.
pub struct Live {
    bot: Bot,
    chat_id: ChatId,
    message: Option<MessageId>,
    last_edit: Option<Instant>,
    shown: String,
}

impl Live {
    pub fn new(bot: Bot, chat_id: ChatId) -> Self {
        Self { bot, chat_id, message: None, last_edit: None, shown: String::new() }
    }

    /// The message being edited, once it exists — needed to remember what was
    /// said so a later 👍 can be resolved to it.
    pub fn message_id(&self) -> Option<MessageId> {
        self.message
    }

    /// Text currently displayed.
    pub fn shown(&self) -> &str {
        &self.shown
    }

    /// Rewrites the message. Skipped when nothing changed or when the last
    /// edit was too recent, unless `force` (the final answer must always
    /// land). Failures are logged, never fatal: losing a progress frame must
    /// not lose the answer.
    pub async fn show(&mut self, text: &str, force: bool) {
        let text = text.trim();
        if text.is_empty() || text == self.shown {
            return;
        }
        if !force && !self.due(Instant::now()) {
            return;
        }
        // Long answers are chunked by the caller; a frame that overflows is
        // clipped rather than dropped.
        let text: String = text.chars().take(crate::text::TELEGRAM_LIMIT).collect();
        let sent = match self.message {
            Some(id) => self
                .bot
                .edit_message_text(self.chat_id, id, text.clone())
                .await
                .map(|m| m.id),
            None => self
                .bot
                .send_message(self.chat_id, text.clone())
                .await
                .map(|m| m.id),
        };
        match sent {
            Ok(id) => {
                self.message = Some(id);
                self.shown = text;
                self.last_edit = Some(Instant::now());
            }
            Err(e) => tracing::debug!(error = %e, "progress update failed"),
        }
    }

    fn due(&self, now: Instant) -> bool {
        self.last_edit
            .is_none_or(|last| now.duration_since(last) >= MIN_EDIT_INTERVAL)
    }
}

/// What to show while a tool runs. The arguments carry the interesting part
/// — which query, which page — so the user can see the search actually
/// widening rather than a generic spinner.
pub fn describe(tool: &str, args: &serde_json::Value) -> String {
    let s = |key: &str| args.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_string();
    match tool {
        "search_web" => {
            let langs = args
                .get("also_queries")
                .and_then(|v| v.as_array())
                .map(|a| a.len() + 1)
                .unwrap_or(1);
            match (s("query"), langs) {
                (q, 1) if !q.is_empty() => format!("🔎 searching: {q}"),
                (q, n) if !q.is_empty() => format!("🔎 searching in {n} languages: {q}"),
                _ => "🔎 searching".to_string(),
            }
        }
        "search_secondhand" => format!("🛒 second-hand: {}", s("query")),
        "fetch_page" => match host(&s("url")) {
            Some(host) => format!("📄 opening {host}"),
            None => "📄 opening a page".to_string(),
        },
        "compare_prices" => {
            let offers = args.get("offers").and_then(|v| v.as_array()).map_or(0, |a| a.len());
            let unit = s("unit_name");
            match (offers, unit.is_empty()) {
                (0, _) => "🧮 comparing prices".to_string(),
                (n, true) => format!("🧮 comparing {n} offers"),
                (n, false) => format!("🧮 comparing {n} offers per {unit}"),
            }
        }
        "query_purchases" => "📚 checking your purchase history".to_string(),
        "record_purchase" => "💾 saving the purchase".to_string(),
        "remember_fact" | "forget_fact" => "💾 updating your profile".to_string(),
        "create_reminder" | "cancel_reminder" | "list_reminders" => "⏰ reminders".to_string(),
        other => format!("⚙️ {other}"),
    }
}

fn host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let host = rest.split('/').next()?.trim_start_matches("www.");
    (!host.is_empty()).then(|| host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_calls_read_as_plain_progress() {
        assert_eq!(
            describe("search_web", &json!({"query": "wasmiddel"})),
            "🔎 searching: wasmiddel"
        );
        assert_eq!(
            describe(
                "search_web",
                &json!({"query": "detergent", "also_queries": ["wasmiddel", "Waschmittel"]})
            ),
            "🔎 searching in 3 languages: detergent"
        );
        assert_eq!(
            describe("fetch_page", &json!({"url": "https://www.bol.com/nl/nl/p/x/123/"})),
            "📄 opening bol.com"
        );
        assert_eq!(
            describe("compare_prices", &json!({"unit_name": "wash", "offers": [1, 2, 3]})),
            "🧮 comparing 3 offers per wash"
        );
        assert_eq!(
            describe("query_purchases", &json!({"search_term": "ariel"})),
            "📚 checking your purchase history"
        );
    }

    #[test]
    fn missing_or_odd_arguments_never_panic() {
        assert_eq!(describe("search_web", &json!({})), "🔎 searching");
        assert_eq!(describe("fetch_page", &json!({"url": "not a url"})), "📄 opening a page");
        assert_eq!(describe("compare_prices", &json!({})), "🧮 comparing prices");
        assert_eq!(describe("brand_new_tool", &json!(null)), "⚙️ brand_new_tool");
    }

    #[test]
    fn edits_are_rate_limited_but_the_first_is_immediate() {
        let mut live = Live::new(Bot::new("token"), ChatId(1));
        let now = Instant::now();
        assert!(live.due(now), "nothing shown yet, so the first frame goes out at once");

        live.last_edit = Some(now);
        assert!(!live.due(now + Duration::from_millis(300)));
        assert!(live.due(now + MIN_EDIT_INTERVAL));
    }
}
