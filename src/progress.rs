//! Live feedback while the agent works.
//!
//! A product search runs a dozen tool calls before it writes a word, so the
//! chat sits silent for the better part of a minute. One message is sent up
//! front and edited as work happens — first with what the agent is doing,
//! then with its answer as it streams in.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use teloxide::prelude::*;
use teloxide::types::{MessageId, ParseMode};

/// Telegram rate-limits edits; this is comfortably under what it tolerates
/// while still feeling live.
const MIN_EDIT_INTERVAL: Duration = Duration::from_millis(1200);

/// What Telegram allows one bot across *all* of its chats, not per chat.
/// teloxide's own throttle adaptor carries the same figure as its default.
const TELEGRAM_EDITS_PER_SECOND: usize = 30;

/// Slice of that budget each reply in flight is entitled to.
///
/// Derived from the ceiling with a couple of milliseconds of margin, so the
/// arithmetic lands under it rather than exactly on it — at a hundred
/// replies an exact share divides out to 30.0003 edits a second, and being
/// right on the line is how you find out where the line really is.
const PER_STREAM: Duration = Duration::from_millis(1000 / TELEGRAM_EDITS_PER_SECOND as u64 + 2);

/// How long to wait between frames, given how many replies are streaming.
///
/// [`MIN_EDIT_INTERVAL`] alone is a per-chat rule, and Telegram's harder
/// limit is per bot: at 1.2s a frame it takes only thirty-six simultaneous
/// replies to saturate the whole token, and the rest come back 429. This
/// starts widening at thirty-five and changes nothing below that; above it
/// every reply slows down together, which is the cheapest fair way to stay
/// inside one shared budget.
///
/// Note that teloxide's `throttle` adaptor would not cover this. It forwards
/// `edit_message_text` to the inner requester unthrottled, and edits are
/// nearly all of what a streaming reply sends.
fn edit_interval(active: usize) -> Duration {
    let active = u32::try_from(active).unwrap_or(u32::MAX);
    MIN_EDIT_INTERVAL.max(PER_STREAM.saturating_mul(active))
}

/// Counts one reply for as long as it is in flight.
///
/// A guard rather than a pair of calls in the handler: a request can end by
/// early return, by `?`, or by panic, and every one of those has to give the
/// slot back. A count that only ever rises would throttle the bot to a
/// standstill and look like Telegram's fault.
struct StreamSlot(Arc<AtomicUsize>);

impl StreamSlot {
    fn take(streams: Arc<AtomicUsize>) -> Self {
        streams.fetch_add(1, Ordering::Relaxed);
        Self(streams)
    }

    fn active(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// How much of the model's reasoning to keep on screen. It runs to
/// paragraphs; the newest sentence or two is the part worth reading.
const THINKING_TAIL: usize = 220;

/// The last `max` characters, cut at a word boundary and marked with a
/// leading ellipsis when anything was dropped.
fn tail(text: &str, max: usize) -> String {
    let text = text.trim();
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let cut: String = text.chars().skip(count - max).collect();
    let cut = cut.split_once(' ').map(|(_, rest)| rest).unwrap_or(&cut);
    format!("…{}", cut.trim_start())
}

/// Telegram's HTML parse mode needs these three escaped; anything else in a
/// reasoning trace is literal.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// One Telegram message, repeatedly rewritten.
pub struct Live {
    bot: Bot,
    chat_id: ChatId,
    message: Option<MessageId>,
    last_edit: Option<Instant>,
    shown: String,
    /// Held for the lifetime of the reply; see [`StreamSlot`].
    slot: StreamSlot,
}

impl Live {
    /// `streams` is the process-wide count of replies in flight, shared by
    /// every `Live`. Telegram's ceiling is per bot token, so pacing can only
    /// be decided from what the whole process is doing.
    pub fn new(bot: Bot, chat_id: ChatId, streams: Arc<AtomicUsize>) -> Self {
        Self {
            bot,
            chat_id,
            message: None,
            last_edit: None,
            shown: String::new(),
            slot: StreamSlot::take(streams),
        }
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
        self.render(text, force, None).await
    }

    /// The model's reasoning, in italics — it is worth watching while the
    /// search runs, but it must never read as part of the answer.
    pub async fn show_thinking(&mut self, text: &str) {
        let text = tail(text, THINKING_TAIL);
        self.render(&text, false, Some(format!("<i>💭 {}</i>", escape_html(&text)))).await
    }

    async fn render(&mut self, text: &str, force: bool, html: Option<String>) {
        let text = text.trim();
        if text.is_empty() || text == self.shown {
            return;
        }
        if !force && !self.due(Instant::now(), self.slot.active()) {
            return;
        }
        // Long answers are chunked by the caller; a frame that overflows is
        // clipped rather than dropped.
        let text: String = text.chars().take(crate::text::TELEGRAM_LIMIT).collect();
        let body = html.clone().unwrap_or_else(|| text.clone());
        let parse_mode = html.is_some().then_some(ParseMode::Html);
        let sent = match self.message {
            Some(id) => {
                let mut req = self.bot.edit_message_text(self.chat_id, id, body);
                req.parse_mode = parse_mode;
                req.await.map(|m| m.id)
            }
            None => {
                let mut req = self.bot.send_message(self.chat_id, body);
                req.parse_mode = parse_mode;
                req.await.map(|m| m.id)
            }
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

    fn due(&self, now: Instant, active: usize) -> bool {
        self.last_edit
            .is_none_or(|last| now.duration_since(last) >= edit_interval(active))
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
    fn thinking_is_tailed_at_a_word_boundary_and_html_safe() {
        assert_eq!(tail("short thought", 220), "short thought");

        let long = "a".repeat(200) + " and then the newest part of the reasoning";
        let cut = tail(&long, 30);
        assert!(cut.starts_with('…'), "got: {cut}");
        assert!(cut.ends_with("newest part of the reasoning"), "got: {cut}");
        assert!(!cut.contains("aaa"), "the old part is gone: {cut}");

        // A reasoning trace quoting markup must not become markup itself.
        assert_eq!(
            escape_html("compare <b>5 L</b> & 3 L > 1 L"),
            "compare &lt;b&gt;5 L&lt;/b&gt; &amp; 3 L &gt; 1 L"
        );
    }

    #[test]
    fn edits_are_rate_limited_but_the_first_is_immediate() {
        let mut live = Live::new(Bot::new("token"), ChatId(1), Arc::new(AtomicUsize::new(0)));
        let now = Instant::now();
        assert!(live.due(now, 1), "nothing shown yet, so the first frame goes out at once");

        live.last_edit = Some(now);
        assert!(!live.due(now + Duration::from_millis(300), 1));
        assert!(live.due(now + MIN_EDIT_INTERVAL, 1));
    }

    #[test]
    fn a_crowd_of_replies_paces_itself_under_telegrams_ceiling() {
        // One reply on its own is unaffected: the interval is what it always
        // was, and the answer still feels live.
        assert_eq!(edit_interval(1), MIN_EDIT_INTERVAL);
        assert_eq!(edit_interval(30), MIN_EDIT_INTERVAL, "below the crossover nothing changes");

        // A hundred replies each editing every 1.2s would be 83 edits a
        // second, nearly three times what Telegram allows one bot across all
        // of its chats. Backing off to 3.5s brings the fleet inside it.
        assert_eq!(edit_interval(100), Duration::from_millis(3500));

        // The property that actually matters, at every size: whatever the
        // load, the whole process stays under Telegram's 30 a second.
        for active in [1usize, 10, 29, 30, 31, 36, 100, 500, 5000] {
            let per_second = active as f64 / edit_interval(active).as_secs_f64();
            assert!(
                per_second <= TELEGRAM_EDITS_PER_SECOND as f64,
                "{active} streams would send {per_second:.1} edits/sec"
            );
        }
    }

    #[test]
    fn a_live_reply_counts_itself_while_it_is_in_flight() {
        let streams = Arc::new(AtomicUsize::new(0));
        {
            let _first = Live::new(Bot::new("token"), ChatId(1), streams.clone());
            assert_eq!(streams.load(Ordering::Relaxed), 1);
            let _second = Live::new(Bot::new("token"), ChatId(2), streams.clone());
            assert_eq!(streams.load(Ordering::Relaxed), 2);
        }
        // Dropped on the way out of the block — including when the handler
        // above it returned early or panicked, which is the whole reason the
        // count lives in a guard rather than in the handler.
        assert_eq!(streams.load(Ordering::Relaxed), 0, "a finished reply frees its slot");
    }
}
