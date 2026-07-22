use crate::agent::{build_agent, AgentDeps, HISTORY_CAP};
use crate::config::Config;
use crate::draft::{resolve_draft, DraftResolution};
use crate::text::{split_message, TELEGRAM_LIMIT};
use crate::vision::describe_photo;
use dashmap::DashMap;
use rig::completion::{Chat, Message as LlmMessage};
use std::sync::Arc;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::ChatAction;
use teloxide::utils::command::BotCommands;

pub struct App {
    pub cfg: Config,
    pub deps: AgentDeps,
    pub chats: DashMap<i64, ChatSession>,
}

#[derive(Default)]
pub struct ChatSession {
    pub history: Vec<LlmMessage>,
    pub pending_draft: Option<String>,
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

Commands:
/reset - forget this conversation
/help - this message";

pub async fn run(bot: Bot, app: Arc<App>) {
    let handler = Update::filter_message()
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

    let pending = app
        .chats
        .get(&chat_id.0)
        .and_then(|c| c.pending_draft.clone());
    let prompt = match resolve_draft(pending.as_deref(), &text) {
        DraftResolution::NoDraft => text,
        DraftResolution::Confirmed(draft) | DraftResolution::Replaced(draft) => {
            if let Some(mut chat) = app.chats.get_mut(&chat_id.0) {
                chat.pending_draft = None;
            }
            format!("Find this product for me: {draft}")
        }
    };

    let _ = bot.send_chat_action(chat_id, ChatAction::Typing).await;

    match run_agent(&app, user_id, chat_id.0, &prompt).await {
        Ok(reply) => send_chunked(&bot, chat_id, &reply).await?,
        Err(e) => {
            tracing::error!(error = %e, chat_id = chat_id.0, "agent request failed");
            bot.send_message(
                chat_id,
                "Sorry, something went wrong on my side. Please try again.",
            )
            .await?;
        }
    }
    Ok(())
}

async fn handle_photo(bot: Bot, msg: Message, app: Arc<App>) -> ResponseResult<()> {
    let chat_id = msg.chat.id;
    // Sizes are ordered smallest to largest; take the largest.
    let Some(photo) = msg.photo().and_then(|sizes| sizes.last()) else {
        return Ok(());
    };
    let _ = bot.send_chat_action(chat_id, ChatAction::Typing).await;

    let file = bot.get_file(photo.file.id.clone()).await?;
    let mut bytes: Vec<u8> = Vec::new();
    bot.download_file(&file.path, &mut bytes).await?;

    match describe_photo(&app.deps.llm, &bytes, msg.caption()).await {
        Ok(draft) => {
            app.chats.entry(chat_id.0).or_default().pending_draft = Some(draft.clone());
            bot.send_message(
                chat_id,
                format!(
                    "Looks like: {draft}\n\nReply 'go' to search, or send a corrected description."
                ),
            )
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

/// Runs the agent against a snapshot of this chat's history, then writes the
/// updated history back (capped). Snapshot-then-writeback keeps DashMap locks
/// from being held across awaits.
async fn run_agent(
    app: &App,
    user_id: i64,
    chat_id: i64,
    prompt: &str,
) -> anyhow::Result<String> {
    let agent = build_agent(&app.deps, user_id, chat_id);
    let mut history = app
        .chats
        .get(&chat_id)
        .map(|c| c.history.clone())
        .unwrap_or_default();

    let reply = agent.chat(prompt, &mut history).await?;

    if history.len() > HISTORY_CAP {
        history.drain(..history.len() - HISTORY_CAP);
    }
    app.chats.entry(chat_id).or_default().history = history;
    Ok(reply)
}

/// Send in <=4096-char chunks; each chunk gets one retry.
async fn send_chunked(bot: &Bot, chat_id: ChatId, text: &str) -> ResponseResult<()> {
    let chunks = split_message(text, TELEGRAM_LIMIT);
    if chunks.is_empty() {
        bot.send_message(chat_id, "(no answer - please try again)").await?;
        return Ok(());
    }
    for chunk in chunks {
        if let Err(e) = bot.send_message(chat_id, chunk.clone()).await {
            tracing::warn!(error = %e, "send failed, retrying once");
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            bot.send_message(chat_id, chunk).await?;
        }
    }
    Ok(())
}
