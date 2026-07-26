use crate::agent::{build_agent, AgentDeps, HISTORY_CAP};
use crate::config::Config;
use crate::draft::{resolve_draft, DraftResolution};
use crate::text::{split_message, strip_thinking, TELEGRAM_LIMIT};
use crate::vision::describe_photo;
use dashmap::DashMap;
use rig::completion::{Chat, Message as LlmMessage};
use std::sync::Arc;
use teloxide::net::Download;
use teloxide::prelude::*;
use std::collections::VecDeque;
use teloxide::types::{
    ChatAction, CopyTextButton, InlineKeyboardButton, InlineKeyboardButtonKind,
    InlineKeyboardMarkup, MessageReactionUpdated, ReactionType,
};
use teloxide::utils::command::BotCommands;

pub struct App {
    pub cfg: Config,
    pub deps: AgentDeps,
    pub chats: DashMap<i64, ChatSession>,
}

/// How many of the bot's own recent replies to keep per chat so reactions
/// (which carry only a message id) can be resolved back to their text.
const SENT_REPLY_CAP: usize = 30;

/// A chat quiet for longer than this starts a fresh session on the next
/// message; for text messages an LLM check may restore the old context if
/// the new message continues the same topic.
const SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

#[derive(Default)]
pub struct ChatSession {
    pub history: Vec<LlmMessage>,
    pub pending_draft: Option<String>,
    /// (message_id, text) of replies this bot sent, newest last.
    pub sent_replies: VecDeque<(i32, String)>,
    /// When this chat last had activity; None until the first message.
    pub last_seen: Option<std::time::Instant>,
}

/// True when the gap since `last_seen` exceeds the session TTL.
fn session_expired(last_seen: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    last_seen.is_some_and(|t| now.duration_since(t) > SESSION_TTL)
}

/// The last `n` plain-text messages of a history, rendered as
/// "user:/assistant:" lines for the continuation classifier.
fn last_messages_text(history: &[LlmMessage], n: usize) -> String {
    let mut lines: Vec<String> = history
        .iter()
        .rev()
        .filter_map(|m| match m {
            LlmMessage::User { content } => content
                .iter()
                .filter_map(|c| match c {
                    rig::message::UserContent::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .reduce(|a, b| format!("{a}\n{b}"))
                .map(|t| format!("user: {t}")),
            LlmMessage::Assistant { content, .. } => content
                .iter()
                .filter_map(|c| match c {
                    rig::message::AssistantContent::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .reduce(|a, b| format!("{a}\n{b}"))
                .map(|t| format!("assistant: {t}")),
            _ => None,
        })
        .take(n)
        .collect();
    lines.reverse();
    lines.join("\n")
}

impl ChatSession {
    pub fn remember_reply(&mut self, message_id: i32, text: &str) {
        self.sent_replies.push_back((message_id, text.to_string()));
        while self.sent_replies.len() > SENT_REPLY_CAP {
            self.sent_replies.pop_front();
        }
    }

    pub fn reply_text(&self, message_id: i32) -> Option<String> {
        self.sent_replies
            .iter()
            .find(|(id, _)| *id == message_id)
            .map(|(_, text)| text.clone())
    }
}

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
enum Command {
    #[command(description = "what this bot does")]
    Start,
    #[command(description = "show help")]
    Help,
    #[command(description = "forget this conversation and any pending photo draft")]
    Reset,
}

const HELP: &str = "\
I'm Scout - I research products for you.

Just tell me what you're looking for (budget, country, must-haves help).
Send a photo of a product and I'll draft a search from it.
Tell me when you bought something and I'll remember where and for how much.
I can remind you when it's time to reorder things you buy regularly.

After 10 quiet minutes I start a fresh conversation automatically (I'll pick
the old one back up if you're clearly continuing the same topic).

Commands:
/reset - forget this conversation right now
/help - this message";

pub async fn run(bot: Bot, app: Arc<App>) {
    let messages = Update::filter_message()
        .filter(|msg: Message, app: Arc<App>| is_allowed(&app, &msg))
        .branch(
            dptree::entry()
                .filter_command::<Command>()
                .endpoint(handle_command),
        )
        .branch(
            dptree::filter(|msg: Message| msg.photo().is_some()).endpoint(handle_photo),
        )
        .branch(
            dptree::filter(|msg: Message| msg.text().is_some()).endpoint(handle_text),
        );
    // Adding this branch makes the dispatcher request message_reaction
    // updates from Telegram automatically (allowed_updates hinting).
    let handler = dptree::entry()
        .branch(messages)
        .branch(Update::filter_message_reaction_updated().endpoint(handle_reaction));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![app])
        .default_handler(|_| async {})
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

fn is_allowed(app: &App, msg: &Message) -> bool {
    sender_id(msg)
        .map(|id| app.cfg.allowed_user_ids.contains(&id))
        .unwrap_or(false)
}

fn sender_id(msg: &Message) -> Option<i64> {
    msg.from.as_ref().map(|u| u.id.0 as i64)
}

async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    app: Arc<App>,
) -> ResponseResult<()> {
    match cmd {
        Command::Start | Command::Help => {
            bot.send_message(msg.chat.id, HELP).await?;
        }
        Command::Reset => {
            app.chats.remove(&msg.chat.id.0);
            bot.send_message(msg.chat.id, "Conversation cleared.").await?;
        }
    }
    Ok(())
}

async fn handle_text(bot: Bot, msg: Message, app: Arc<App>) -> ResponseResult<()> {
    let text = msg.text().unwrap_or_default().to_string();
    let chat_id = msg.chat.id;
    let Some(user_id) = sender_id(&msg) else { return Ok(()) };

    // Session expiry: after a long gap the old context is set aside; a quick
    // LLM check restores it when the new message continues the same topic.
    let stale_history = {
        let mut chat = app.chats.entry(chat_id.0).or_default();
        let now = std::time::Instant::now();
        let expired = session_expired(chat.last_seen, now) && !chat.history.is_empty();
        chat.last_seen = Some(now);
        if expired {
            chat.pending_draft = None;
            Some(std::mem::take(&mut chat.history))
        } else {
            None
        }
    };
    if let Some(old_history) = stale_history {
        let excerpt = last_messages_text(&old_history, 6);
        match crate::agent::continues_previous(&app.deps.llm, &excerpt, &text).await {
            Ok(true) => {
                tracing::info!(chat_id = chat_id.0, "session expired but topic continues; restoring context");
                app.chats.entry(chat_id.0).or_default().history = old_history;
            }
            Ok(false) => {
                tracing::info!(chat_id = chat_id.0, "session expired; starting fresh");
            }
            Err(e) => {
                tracing::warn!(error = %e, chat_id = chat_id.0,
                    "continuation check failed; starting fresh");
            }
        }
    }

    let pending = app
        .chats
        .get(&chat_id.0)
        .and_then(|c| c.pending_draft.clone());
    let resolution = resolve_draft(pending.as_deref(), &text);
    let had_draft = !matches!(resolution, DraftResolution::NoDraft);
    if had_draft {
        if let Some(mut chat) = app.chats.get_mut(&chat_id.0) {
            chat.pending_draft = None;
        }
    }
    let prompt = match resolution {
        DraftResolution::NoDraft => text,
        DraftResolution::Cancelled => {
            bot.send_message(chat_id, "Okay, dropped it.").await?;
            return Ok(());
        }
        DraftResolution::Confirmed(draft) | DraftResolution::Replaced(draft) => {
            format!("Find this product for me: {draft}")
        }
    };

    let _ = bot.send_chat_action(chat_id, ChatAction::Typing).await;

    match run_agent(&app, user_id, chat_id.0, &prompt).await {
        Ok(reply) => send_chunked(&bot, &app, chat_id, &reply).await?,
        Err(e) => {
            tracing::error!(error = %e, chat_id = chat_id.0, "agent request failed");
            bot.send_message(chat_id, agent_error_message(&e)).await?;
        }
    }
    Ok(())
}

/// Turn an agent failure into a user-facing message; the max-turns budget
/// gets an actionable explanation instead of the generic apology.
fn agent_error_message(e: &anyhow::Error) -> &'static str {
    if e.to_string().contains("max turns") {
        "That request needed more research steps than I allow per message. \
         Try narrowing it (a more specific product, or fewer platforms), or \
         ask me to continue from where I stopped."
    } else {
        "Sorry, something went wrong on my side. Please try again."
    }
}

async fn handle_photo(bot: Bot, msg: Message, app: Arc<App>) -> ResponseResult<()> {
    let chat_id = msg.chat.id;
    {
        // A photo after a long gap always starts fresh (no continuation
        // check — a new photo is a new product hunt), and any stale draft
        // is dropped either way.
        let mut chat = app.chats.entry(chat_id.0).or_default();
        let now = std::time::Instant::now();
        if session_expired(chat.last_seen, now) {
            chat.history.clear();
        }
        chat.last_seen = Some(now);
        chat.pending_draft = None;
    }
    // Sizes are ordered smallest to largest; take the largest.
    let Some(photo) = msg.photo().and_then(|sizes| sizes.last()) else {
        return Ok(());
    };
    let _ = bot.send_chat_action(chat_id, ChatAction::Typing).await;

    let bytes = match download_photo(&bot, photo.file.id.clone()).await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!(error = %e, chat_id = chat_id.0, "photo download failed");
            bot.send_message(
                chat_id,
                "Sorry, I couldn't download that photo. Please try again.",
            )
            .await?;
            return Ok(());
        }
    };

    match describe_photo(&app.deps.llm, &bytes, msg.caption()).await {
        Ok(draft) => {
            app.chats.entry(chat_id.0).or_default().pending_draft = Some(draft.clone());
            bot.send_message(
                chat_id,
                format!(
                    "Looks like: {draft}\n\nReply 'go' to search as-is, or tap the \
                     button to copy the text, then paste, edit and send it."
                ),
            )
            .reply_markup(copy_draft_markup(&draft))
            .await?;
        }
        Err(e) => {
            tracing::error!(error = %e, chat_id = chat_id.0, "photo description failed");
            bot.send_message(
                chat_id,
                "Sorry, I couldn't make sense of that photo. Try describing the product in text.",
            )
            .await?;
        }
    }
    Ok(())
}

/// A 👍 on one of the bot's replies means "I'm considering buying this" —
/// resolve the reacted message's text and let the agent offer to save the
/// right product to purchase memory.
async fn handle_reaction(
    bot: Bot,
    reaction: MessageReactionUpdated,
    app: Arc<App>,
) -> ResponseResult<()> {
    tracing::info!(
        chat_id = reaction.chat.id.0,
        message_id = reaction.message_id.0,
        new_reaction = ?reaction.new_reaction,
        "reaction update received"
    );
    let Some(user) = reaction.user() else { return Ok(()) };
    let user_id = user.id.0 as i64;
    if !app.cfg.allowed_user_ids.contains(&user_id) {
        return Ok(());
    }
    if !thumbs_up_added(&reaction.old_reaction, &reaction.new_reaction) {
        tracing::debug!("not a newly added thumbs-up; ignoring");
        return Ok(());
    }
    let chat_id = reaction.chat.id;
    let Some(text) = app
        .chats
        .get(&chat_id.0)
        .and_then(|c| c.reply_text(reaction.message_id.0))
    else {
        // Reacted to something we no longer (or never) tracked — stay quiet
        // toward the user (the cache is in-memory, so replies from before the
        // last restart can't be resolved).
        tracing::info!(
            message_id = reaction.message_id.0,
            "thumbs-up on an untracked message (sent before last restart?); ignoring"
        );
        return Ok(());
    };

    let _ = bot.send_chat_action(chat_id, ChatAction::Typing).await;
    let prompt = format!(
        "[system note] The user reacted with a thumbs-up to this earlier reply \
         of yours:\n---\n{text}\n---\nThat means they are considering buying \
         one of the products in it. If the reply contains exactly one product, \
         ask them to confirm saving it to purchase memory (confirm store and \
         price while you're at it). If it contains several, list them as a \
         short numbered list and ask which one to save. Do NOT call \
         record_purchase until they confirm."
    );
    match run_agent(&app, user_id, chat_id.0, &prompt).await {
        Ok(reply) => send_chunked(&bot, &app, chat_id, &reply).await?,
        Err(e) => {
            tracing::error!(error = %e, chat_id = chat_id.0, "reaction follow-up failed");
        }
    }
    Ok(())
}

fn thumbs_up_added(old: &[ReactionType], new: &[ReactionType]) -> bool {
    let has_thumb = |rs: &[ReactionType]| {
        rs.iter()
            .any(|r| matches!(r, ReactionType::Emoji { emoji } if emoji == "👍"))
    };
    !has_thumb(old) && has_thumb(new)
}

/// Downloads a Telegram file by id into memory.
async fn download_photo(
    bot: &Bot,
    file_id: teloxide::types::FileId,
) -> anyhow::Result<Vec<u8>> {
    let file = bot.get_file(file_id).await?;
    let mut bytes: Vec<u8> = Vec::new();
    bot.download_file(&file.path, &mut bytes).await?;
    Ok(bytes)
}

/// Runs the agent against a snapshot of this chat's history, then writes the
/// updated history back (capped). Snapshot-then-writeback keeps DashMap locks
/// from being held across awaits.
async fn run_agent(
    app: &App,
    user_id: i64,
    chat_id: i64,
    prompt: &str,
) -> anyhow::Result<String> {
    let facts = {
        let store = app.deps.store.clone();
        tokio::task::spawn_blocking(move || store.list_facts(user_id)).await??
    };
    let agent = build_agent(&app.deps, user_id, chat_id, &facts);
    let mut history = app
        .chats
        .get(&chat_id)
        .map(|c| c.history.clone())
        .unwrap_or_default();

    let reply = strip_thinking(&agent.chat(prompt, &mut history).await?);

    trim_history(&mut history, HISTORY_CAP);
    app.chats.entry(chat_id).or_default().history = history;
    Ok(reply)
}

/// Caps `history` at `cap` messages, then trims further so it never starts
/// mid tool-call/tool-result exchange. Providers serialize history as-is, and
/// a leading orphaned tool-result message (a user-role message whose content
/// is a `ToolResult` left behind when its assistant tool-call got cut) is
/// rejected by strict OpenAI-compatible backends. We only trust a history
/// that begins with a plain user text message.
pub(crate) fn trim_history(history: &mut Vec<LlmMessage>, cap: usize) {
    if history.len() <= cap {
        return;
    }
    history.drain(..history.len() - cap);
    match history.iter().position(is_plain_user_text) {
        Some(0) => {}
        Some(i) => {
            history.drain(..i);
        }
        None => history.clear(),
    }
}

/// A `Message::User` whose content is entirely plain text - no tool-result,
/// image, or other part that only makes sense following a tool call.
fn is_plain_user_text(msg: &LlmMessage) -> bool {
    matches!(
        msg,
        LlmMessage::User { content }
            if content.iter().all(|c| !matches!(c, rig::message::UserContent::ToolResult(_)))
    )
}

/// Inline button under a photo draft: tapping copies the draft into the
/// clipboard so the user can paste, edit and send it. Bot API caps
/// copyable text at 256 chars.
fn copy_draft_markup(draft: &str) -> InlineKeyboardMarkup {
    let text: String = draft.chars().take(256).collect();
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::new(
        "📋 Copy to edit",
        InlineKeyboardButtonKind::CopyText(CopyTextButton { text }),
    )]])
}

/// Send in <=4096-char chunks; each chunk gets one retry. Sent chunks are
/// remembered per chat so a later reaction on one can be resolved to its
/// text.
async fn send_chunked(bot: &Bot, app: &App, chat_id: ChatId, text: &str) -> ResponseResult<()> {
    let chunks = split_message(text, TELEGRAM_LIMIT);
    if chunks.is_empty() {
        bot.send_message(chat_id, "(no answer - please try again)").await?;
        return Ok(());
    }
    for chunk in chunks {
        let sent = match bot.send_message(chat_id, chunk.clone()).await {
            Ok(sent) => sent,
            Err(e) => {
                tracing::warn!(error = %e, "send failed, retrying once");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                bot.send_message(chat_id, chunk.clone()).await?
            }
        };
        app.chats
            .entry(chat_id.0)
            .or_default()
            .remember_reply(sent.id.0, &chunk);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{thumbs_up_added, ChatSession, SENT_REPLY_CAP};
    use teloxide::types::ReactionType;

    fn thumb() -> ReactionType {
        ReactionType::Emoji { emoji: "👍".to_string() }
    }

    fn heart() -> ReactionType {
        ReactionType::Emoji { emoji: "❤".to_string() }
    }

    #[test]
    fn thumbs_up_detection_only_fires_on_newly_added_thumb() {
        assert!(thumbs_up_added(&[], &[thumb()]));
        assert!(thumbs_up_added(&[heart()], &[heart(), thumb()]));
        // removing, unrelated reactions, or pre-existing thumb: no trigger
        assert!(!thumbs_up_added(&[thumb()], &[]));
        assert!(!thumbs_up_added(&[], &[heart()]));
        assert!(!thumbs_up_added(&[thumb()], &[thumb(), heart()]));
    }

    #[test]
    fn session_expiry_boundary() {
        use super::{session_expired, SESSION_TTL};
        use std::time::Instant;
        let now = Instant::now();
        assert!(!session_expired(None, now), "first contact is never stale");
        assert!(!session_expired(Some(now - SESSION_TTL / 2), now));
        assert!(session_expired(
            Some(now - SESSION_TTL - std::time::Duration::from_secs(1)),
            now
        ));
    }

    #[test]
    fn last_messages_text_renders_roles_and_takes_tail() {
        use super::last_messages_text;
        let history = vec![
            user_text("oldest question"),
            assistant_text("oldest answer"),
            user_text("find me a bike"),
            assistant_text("here are 3 bikes"),
        ];
        let excerpt = last_messages_text(&history, 2);
        assert_eq!(excerpt, "user: find me a bike\nassistant: here are 3 bikes");
    }

    #[test]
    fn reply_cache_resolves_and_caps() {
        let mut session = ChatSession::default();
        for i in 0..(SENT_REPLY_CAP as i32 + 5) {
            session.remember_reply(i, &format!("reply {i}"));
        }
        assert_eq!(session.sent_replies.len(), SENT_REPLY_CAP);
        // oldest evicted, newest resolvable
        assert_eq!(session.reply_text(0), None);
        assert_eq!(session.reply_text(34), Some("reply 34".to_string()));
    }

    use super::*;
    use rig::completion::message::{AssistantContent, ToolResult, ToolResultContent, UserContent};
    use rig::message::ToolCall;
    use rig::one_or_many::OneOrMany;

    fn user_text(s: &str) -> LlmMessage {
        LlmMessage::user(s)
    }

    fn assistant_text(s: &str) -> LlmMessage {
        LlmMessage::assistant(s)
    }

    fn assistant_tool_call(id: &str, name: &str) -> LlmMessage {
        LlmMessage::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::ToolCall(ToolCall::new(
                id.to_string(),
                rig::message::ToolFunction {
                    name: name.to_string(),
                    arguments: serde_json::json!({}),
                },
            ))),
        }
    }

    fn tool_result(id: &str) -> LlmMessage {
        LlmMessage::User {
            content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                id: id.to_string(),
                call_id: None,
                content: OneOrMany::one(ToolResultContent::text("result")),
            })),
        }
    }

    #[test]
    fn under_cap_is_untouched() {
        let mut history = vec![user_text("hi"), assistant_text("hello")];
        let original = history.clone();
        trim_history(&mut history, 10);
        assert_eq!(history, original);
    }

    #[test]
    fn at_cap_is_untouched() {
        let mut history = vec![user_text("hi"), assistant_text("hello")];
        let original = history.clone();
        trim_history(&mut history, 2);
        assert_eq!(history, original);
    }

    #[test]
    fn over_cap_trims_to_cap_and_starts_on_plain_user_text() {
        // Clean exchanges only, so the cap boundary itself lands on a user
        // text message - no further trimming needed.
        let mut history = vec![
            user_text("q1"),
            assistant_text("a1"),
            user_text("q2"),
            assistant_text("a2"),
            user_text("q3"),
            assistant_text("a3"),
        ];
        trim_history(&mut history, 4);
        assert!(history.len() <= 4);
        assert!(is_plain_user_text(&history[0]), "got: {:?}", history[0]);
        assert_eq!(history.last(), Some(&assistant_text("a3")));
    }

    #[test]
    fn drain_landing_on_tool_result_trims_forward_to_next_user_text() {
        // Cap of 4 lands the drain boundary right on the tool-result message
        // (index 2), which must not be kept as the new head.
        let mut history = vec![
            user_text("q1"),                        // 0 - dropped by cap
            assistant_tool_call("call-1", "search"), // 1 - dropped by cap
            tool_result("call-1"),                   // 2 - would be new head; orphaned, must drop
            assistant_text("a1"),                    // 3 - also orphaned (no leading user turn)
            user_text("q2"),                         // 4 - first safe head
            assistant_text("a2"),                    // 5
        ];
        trim_history(&mut history, 4);
        assert_eq!(history, vec![user_text("q2"), assistant_text("a2")]);
        assert!(is_plain_user_text(&history[0]));
    }

    #[test]
    fn no_safe_head_after_drain_yields_empty_history_without_panicking() {
        let mut history = vec![
            assistant_tool_call("call-1", "search"),
            tool_result("call-1"),
            assistant_text("final answer"),
        ];
        trim_history(&mut history, 1);
        assert!(history.is_empty());
    }
}
