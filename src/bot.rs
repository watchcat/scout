use crate::agent::{build_agent, wrap_up_agent, HISTORY_CAP, WRAP_UP_NOTE};
use crate::draft::{resolve_draft, DraftResolution};
use crate::progress::Live;
use crate::text::{split_message, strip_thinking, TELEGRAM_LIMIT};
use crate::vision::describe_photo;
use dashmap::DashMap;
use futures::StreamExt;
use rig::agent::MultiTurnStreamItem;
use rig::completion::{Chat, Message as LlmMessage};
use rig::streaming::{StreamedAssistantContent, StreamingChat};
use std::sync::Arc;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{
    ChatAction, CopyTextButton, InlineKeyboardButton, InlineKeyboardButtonKind,
    InlineKeyboardMarkup, MessageReactionUpdated, ParseMode, ReactionType,
};
use teloxide::utils::command::BotCommands;

pub struct App {
    /// Everything that is Scout rather than Telegram. Shared, never owned:
    /// in 2b-2 it lives in another process entirely.
    pub core: Arc<crate::core::Core>,
    /// One entry per (chat_id, sender_id). In a 1:1 chat a single user always
    /// hits the same slot; in a group/supergroup, each allowed user has
    /// their own history, draft and last_seen, isolating conversation
    /// context across tenants.
    pub chats: DashMap<(i64, i64), ChatSession>,
    /// Bot replies are visible to everyone in a chat, so a 👍 on them is
    /// resolved against a chat-scoped (not user-scoped) ring buffer.
    /// Separate from `chats` so removing one user's session doesn't drop
    /// another user's ability to react to historical replies.
    pub replies: DashMap<i64, std::collections::VecDeque<(i32, String)>>,
    /// Replies streaming right now, across every chat. Telegram's throughput
    /// limit is per bot token rather than per chat, so progress edits pace
    /// themselves against this rather than against their own chat alone.
    pub streams: Arc<std::sync::atomic::AtomicUsize>,
    /// Everyone admitted through an invite round and not since revoked,
    /// loaded from `members` at startup. The table is the durable record;
    /// this is what the gate reads.
    ///
    /// Not premature caching: the gate runs on *every* update, including
    /// every message from someone who was never invited. Reading DuckDB
    /// there would take the connection mutex on each one, so anybody who
    /// found the bot could contend with real work just by typing at it.
    /// Disk is touched only when membership actually changes.
    pub members: dashmap::DashSet<i64>,
}

/// How many of the bot's own recent replies to keep per chat so reactions
/// (which carry only a message id) can be resolved back to their text.
const SENT_REPLY_CAP: usize = 30;

/// A chat quiet for longer than this starts a fresh session on the next
/// message; for text messages an LLM check may restore the old context if
/// the new message continues the same topic.

/// Key for the per-(chat,user) session map. Extracted here so the same
/// tuple is built everywhere; in 1:1 chats the same value is reused.
fn chat_key(chat_id: i64, user_id: i64) -> (i64, i64) {
    (chat_id, user_id)
}

#[derive(Default)]
pub struct ChatSession {
    pub pending_draft: Option<String>,
    /// When this chat last had activity; None until the first message.
    pub last_seen: Option<std::time::Instant>,
}

/// True when the gap since `last_seen` exceeds the session TTL.
fn session_expired(last_seen: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    last_seen.is_some_and(|t| now.duration_since(t) > crate::session::SESSION_TTL)
}

/// Apply session expiry to one slot and hand back the aged-out history, if
/// there was any, for the continuation check to look at.
///
/// The slot belongs to a single (chat, user) pair, so this only ever touches
/// the caller's own context. A draft is dropped whenever the session ages
/// out, whether or not the history comes back: otherwise a photo drafted
/// before the gap stays armed and the next bare "ok" searches for it.
fn take_expired_session(chat: &mut ChatSession, now: std::time::Instant) -> bool {
    let expired = session_expired(chat.last_seen, now);
    chat.last_seen = Some(now);
    if expired {
        chat.pending_draft = None;
    }
    expired
}


/// Stash a sent reply against the per-chat ring buffer. Every bot reply in
/// `chat_id` is appended, with FIFO eviction past `SENT_REPLY_CAP`. Used by
/// 👍 reactions to resolve a message id back to its text.
fn remember_chat_reply(
    replies: &DashMap<i64, std::collections::VecDeque<(i32, String)>>,
    chat_id: i64,
    message_id: i32,
    text: &str,
) {
    let mut entry = replies.entry(chat_id).or_default();
    entry.push_back((message_id, text.to_string()));
    while entry.len() > SENT_REPLY_CAP {
        entry.pop_front();
    }
}

/// Look up a replied-to bot message by id in the per-chat ring buffer.
fn chat_reply_text(
    replies: &DashMap<i64, std::collections::VecDeque<(i32, String)>>,
    chat_id: i64,
    message_id: i32,
) -> Option<String> {
    replies
        .get(&chat_id)
        .and_then(|r| r.iter().find(|(id, _)| *id == message_id).map(|(_, t)| t.clone()))
}

/// `/start` is deliberately absent: it is the one command that has to reach
/// people the gate rejects, so it is routed by `is_start` on its own branch
/// ahead of this enum. Leaving a variant here that the router never feeds
/// would let a member's `/start` fall through to the LLM.
#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
enum Command {
    #[command(description = "show help")]
    Help,
    #[command(description = "forget this conversation and any pending photo draft")]
    Reset,
    #[command(description = "usage statistics: /stat [days]")]
    Stat(String),
    #[command(description = "admin only: send a message to everyone: /advert <text>")]
    Advert(String),
    #[command(description = "admin only: invite rounds: /invite new|status|open|close|announce")]
    Invite(String),
    #[command(description = "admin only: remove a member: /kick <user_id>")]
    Kick(String),
    #[command(description = "admin only: undo a kick: /unkick <user_id>")]
    Unkick(String),
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
/stat [days] - your usage statistics (default 7 days)
/help - this message";

/// Appended to `/help` for admins only, so the commands are discoverable by
/// the person who can use them without advertising themselves to everyone
/// else.
const ADMIN_HELP: &str = "\n\
/advert <text> - send an announcement to everyone (admin)\n\
/invite new <name> [capacity] - open a round and get its link (admin)\n\
/invite status - rounds, seats used, and who is waiting (admin)\n\
/invite open|close <name> - resume or stop admitting (admin)\n\
/invite announce <name> - tell the waitlist a round is open (admin)\n\
/kick <user_id> - remove a member (admin)\n\
/unkick <user_id> - undo a kick (admin)";

/// What a stranger who sends a bare `/start` is told. The one message a
/// non-member gets unprompted: silence here reads as broken rather than as
/// closed, and pressing START is the single most likely thing a person does
/// with a bot link.
///
/// It does *not* tell them to go and open an invite link. By the time
/// anyone reads this they have messaged Scout — this reply is the answer to
/// their own `/start` — and a chat with history never shows the START
/// button again, so a link would carry no code. Sending the command by hand
/// is the route that still works for them.
const INVITE_ONLY: &str = "Scout is invite-only right now.\n\n\
If you have an invite code, send it to me like this:\n/start your-code-here";

/// Shown when a round cannot take them — full, closed, or a code that never
/// existed. One reply for all three: which it was is information with no
/// use to the person reading it, and telling them would say whether a code
/// they guessed exists.
const ROUND_FULL: &str = "All invites are gone — wait for the next round. \
I'll message you here when one opens.";

const ACCESS_REMOVED: &str = "Your access was removed.";

/// A claim that could not be written down is refused rather than granted.
/// Failing closed is right here: a failure that admits people is a failure
/// that overfills the round.
const CLAIM_FAILED: &str = "Sorry, something went wrong on my side. \
Please try that link again in a minute.";

pub async fn run(bot: Bot, app: Arc<App>) {
    // `/start` is a sibling of the gate, not a child of it: the join path
    // has to reach people the gate would reject. The branch owns the whole
    // `/start` surface — payload or not — because splitting it would leave
    // a stranger's bare `/start` falling through to a gate that drops it
    // silently, and one handler with five cases means no message can take
    // the wrong path.
    let messages = Update::filter_message()
        .branch(dptree::filter(|msg: Message| is_start(&msg)).endpoint(handle_start))
        .branch(
            dptree::entry()
                .filter(|msg: Message, app: Arc<App>| is_member(&app, &msg))
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
                ),
        );
    // Adding this branch makes the dispatcher request message_reaction
    // updates from Telegram automatically (allowed_updates hinting).
    let handler = dptree::entry()
        .branch(messages)
        .branch(Update::filter_message_reaction_updated().endpoint(handle_reaction));

    let mut dispatcher = Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![app])
        .default_handler(|_| async {})
        .enable_ctrlc_handler()
        .build();

    // A deploy must not cut someone off mid-answer. On shutdown the
    // dispatcher stops taking new updates and then awaits every handler
    // still running, so a request that has already started gets to finish.
    // Ctrl-C is covered above; this is the one Docker actually sends.
    #[cfg(unix)]
    {
        let shutdown = dispatcher.shutdown_token();
        tokio::spawn(async move {
            let mut term =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(term) => term,
                    Err(e) => {
                        tracing::error!(error = %e, "no SIGTERM handler; deploys will cut requests off");
                        return;
                    }
                };
            term.recv().await;
            tracing::info!("SIGTERM received; finishing in-flight requests before exit");
            let started = std::time::Instant::now();
            if let Ok(drained) = shutdown.shutdown() {
                drained.await;
            }
            tracing::info!(seconds = started.elapsed().as_secs(), "in-flight requests finished");
        });
    }

    dispatcher.dispatch().await;
}

fn is_member(app: &App, msg: &Message) -> bool {
    sender_id(msg).is_some_and(|id| is_member_id(app, id))
}

/// The gate: founders from `ALLOWED_TELEGRAM_USER_IDS`, plus everyone
/// admitted through an invite round and not since revoked.
fn is_member_id(app: &App, user_id: i64) -> bool {
    app.core.cfg.allowed_user_ids.contains(&user_id) || app.members.contains(&user_id)
}

/// True for `/start`, with or without an `@bot` suffix and with or without
/// a payload — the whole `/start` surface, since one handler answers all of
/// it.
fn is_start(msg: &Message) -> bool {
    msg.text().is_some_and(|t| start_payload(t).is_some())
}

/// `Some(payload)` — possibly empty — when `text` is a `/start` command.
fn start_payload(text: &str) -> Option<&str> {
    let text = text.trim();
    let (head, rest) = match text.split_once(char::is_whitespace) {
        Some((head, rest)) => (head, rest.trim()),
        None => (text, ""),
    };
    let suffix = head.strip_prefix("/start")?;
    // "/start@scout_bot" is the command; "/started" is a word.
    if !suffix.is_empty() && !suffix.starts_with('@') {
        return None;
    }
    Some(rest)
}

/// The invite code carried by a `/start` message, if it carries one.
fn join_code(text: &str) -> Option<&str> {
    start_payload(text).filter(|p| !p.is_empty())
}

fn sender_id(msg: &Message) -> Option<i64> {
    msg.from.as_ref().map(|u| u.id.0 as i64)
}

/// What `/stat` shows next to a user id. `@handle` when there is one, since
/// that is what people recognise; otherwise the Telegram first/last name.
fn display_name(user: &teloxide::types::User) -> String {
    if let Some(handle) = &user.username {
        return format!("@{handle}");
    }
    match &user.last_name {
        Some(last) => format!("{} {last}", user.first_name),
        None => user.first_name.clone(),
    }
}

/// `/help`'s text for one person: admins get their own commands appended.
fn help_for(app: &App, user_id: i64) -> String {
    match app.core.cfg.admin_user_ids.contains(&user_id) {
        true => format!("{HELP}{ADMIN_HELP}"),
        false => HELP.to_string(),
    }
}

/// The whole `/start` surface, for members and strangers alike — this is the
/// one handler that runs ahead of the gate.
///
/// Five cases: someone already in, a bare start from a stranger, and the
/// three outcomes of claiming a seat.
async fn handle_start(bot: Bot, msg: Message, app: Arc<App>) -> ResponseResult<()> {
    let chat_id = msg.chat.id;
    let Some(user_id) = sender_id(&msg) else { return Ok(()) };
    let code = msg.text().and_then(join_code).map(str::to_string);

    // Founders and members: `/start` is just help, as it always was.
    if is_member_id(&app, user_id) {
        note_sender(&app, &msg);
        let intro = match code.is_some() {
            true => "You're already in.\n\n",
            false => "",
        };
        bot.send_message(chat_id, format!("{intro}{}", help_for(&app, user_id))).await?;
        return Ok(());
    }

    let Some(code) = code else {
        // A stranger's bare `/start`. Everything else they send is met with
        // silence; this is the deliberate exception.
        bot.send_message(chat_id, INVITE_ONLY).await?;
        return Ok(());
    };

    let claim = {
        let store = app.core.deps.store.clone();
        let code = code.clone();
        tokio::task::spawn_blocking(move || {
            let account_id = store.account_for_telegram(user_id)?;
            store.claim_seat(account_id, chat_id.0, &code)
        })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r)
    };
    let claim = match claim {
        Ok(claim) => claim,
        Err(e) => {
            tracing::error!(error = %e, user_id, code, "could not record an invite claim");
            bot.send_message(chat_id, CLAIM_FAILED).await?;
            return Ok(());
        }
    };
    tracing::info!(user_id, code, outcome = ?claim, "invite claim");

    match claim {
        crate::store::Claim::Admitted => {
            app.members.insert(user_id);
            // Only now: `user_chats` is /advert's address book, and a
            // person turned away at the door has not used this bot.
            note_sender(&app, &msg);
            bot.send_message(
                chat_id,
                format!("You're in — welcome to Scout.\n\n{}", help_for(&app, user_id)),
            )
            .await?;
        }
        // The table is the authority when it and the set disagree, so a
        // claim that says they are in puts them in.
        crate::store::Claim::AlreadyIn => {
            app.members.insert(user_id);
            note_sender(&app, &msg);
            bot.send_message(
                chat_id,
                format!("You're already in.\n\n{}", help_for(&app, user_id)),
            )
            .await?;
        }
        crate::store::Claim::Revoked => {
            bot.send_message(chat_id, ACCESS_REMOVED).await?;
        }
        crate::store::Claim::NoRoom => {
            bot.send_message(chat_id, ROUND_FULL).await?;
        }
    }
    Ok(())
}

async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    app: Arc<App>,
) -> ResponseResult<()> {
    // Commands are not counted as requests, but they do tell us who is
    // asking — without this, running /stat never records your own name.
    note_sender(&app, &msg);
    match cmd {
        Command::Help => {
            let help = sender_id(&msg)
                .map(|id| help_for(&app, id))
                .unwrap_or_else(|| HELP.to_string());
            bot.send_message(msg.chat.id, help).await?;
        }
        Command::Invite(arg) => handle_invite(&bot, &msg, &app, &arg).await?,
        Command::Kick(arg) => handle_kick(&bot, &msg, &app, &arg, true).await?,
        Command::Unkick(arg) => handle_kick(&bot, &msg, &app, &arg, false).await?,
        Command::Reset => {
            // Only the caller's session in this chat is cleared; other
            // allowed users keep theirs intact.
            let Some(user_id) = sender_id(&msg) else {
                tracing::debug!("/reset from a message with no sender; ignoring");
                return Ok(());
            };
            app.chats.remove(&chat_key(msg.chat.id.0, user_id));
            // History lives in the store now, so clearing the slot alone
            // would leave the thread intact and /reset would do nothing.
            // A fresh conversation is what "cleared" has to mean.
            let scope = crate::session::conversation_scope(msg.chat.id.0, user_id);
            let store = app.core.deps.store.clone();
            let started = blocking(move || {
                let account_id = store.account_for_telegram(user_id)?;
                store.start_conversation(account_id, &scope)
            })
            .await;
            if let Err(e) = started {
                tracing::error!(error = %e, user_id, "could not clear the conversation");
                bot.send_message(msg.chat.id, CLAIM_FAILED).await?;
                return Ok(());
            }
            bot.send_message(msg.chat.id, "Conversation cleared.").await?;
        }
        Command::Advert(body) => {
            let Some(user_id) = sender_id(&msg) else { return Ok(()) };
            // Same gate as the cross-user /stat view: the admin list is
            // the whole access-control surface for anything that reaches
            // beyond the caller.
            if !app.core.is_admin(user_id) {
                bot.send_message(msg.chat.id, crate::invites::NOT_ADMIN).await?;
                return Ok(());
            }
            let body = match check_advert(&body) {
                Ok(body) => body.to_string(),
                Err(problem) => {
                    bot.send_message(msg.chat.id, problem).await?;
                    return Ok(());
                }
            };

            let store = app.core.store();
            let targets = blocking(move || crate::invites::advert_targets(&store)).await;
            let targets = match targets {
                Ok(targets) => targets,
                Err(e) => {
                    tracing::error!(error = %e, "could not read broadcast targets");
                    bot.send_message(msg.chat.id, "Sorry, couldn't work out who to send to.")
                        .await?;
                    return Ok(());
                }
            };

            let from = msg
                .from
                .as_ref()
                .map(display_name)
                .unwrap_or_else(|| "the admin".to_string());
            let text = advert_message(&body, &from);
            let results = broadcast(&bot, &targets, &text, None).await;
            let sent = results.iter().filter(|(_, d)| *d == Delivered::Ok).count();
            let failed: Vec<i64> = results
                .iter()
                .filter(|(_, d)| *d != Delivered::Ok)
                .map(|(recipient, _)| *recipient)
                .collect();

            let mut report = format!("Sent to {sent} chat(s).");
            if !failed.is_empty() {
                report.push_str(&format!(
                    "\nCould not reach {}: {}",
                    failed.len(),
                    failed.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(", ")
                ));
            }
            bot.send_message(msg.chat.id, report).await?;
        }
        Command::Stat(arg) => {
            let Some(user_id) = sender_id(&msg) else {
                tracing::debug!("/stat from a message with no sender; ignoring");
                return Ok(());
            };
            // Ask Telegram for any display names we are missing before
            // reporting, so the table reads as people rather than numbers.
            backfill_names(&bot, &app).await;
            let report = crate::stats::report(&app.core, user_id, &arg).await;
            bot.send_message(msg.chat.id, format!("<pre>{report}</pre>"))
                .parse_mode(ParseMode::Html)
                .await?;
        }
    }
    Ok(())
}

/// How many names one `/stat` will look up. Bounded because each is a
/// round trip to Telegram; the answers are stored, so a second `/stat`
/// picks up where this one stopped.
const NAME_BACKFILL_LIMIT: usize = 25;

/// Fills in display names Scout has never recorded, by asking Telegram.
///
/// Takes both ids from the store: the Telegram one to ask with, the account
/// one to file under. Deriving one from the other is what previously turned a
/// stats report into `get_chat(ChatId(3))`.
async fn backfill_names(bot: &Bot, app: &Arc<App>) {
    let store = app.core.store();
    let missing = match blocking(move || store.accounts_missing_display_names(NAME_BACKFILL_LIMIT)).await
    {
        Ok(missing) => missing,
        Err(e) => {
            tracing::debug!(error = %e, "could not list accounts missing a name");
            return;
        }
    };
    for (account_id, telegram_id) in missing {
        let chat = match bot.get_chat(ChatId(telegram_id)).await {
            Ok(chat) => chat,
            // Blocked the bot, deleted account, never a private chat —
            // none of it is worth failing /stat over.
            Err(e) => {
                tracing::debug!(error = %e, telegram_id, "could not look up a display name");
                continue;
            }
        };
        let Some(name) = chat_display_name(&chat) else { continue };
        let store = app.core.store();
        if let Err(e) = blocking(move || store.remember_user(account_id, &name)).await {
            tracing::debug!(error = %e, account_id, "could not record a display name");
        }
    }
}

/// The `/stat` label for a chat fetched by id, matching `display_name` for
/// a `User`. Only private chats carry a person's name.
fn chat_display_name(chat: &teloxide::types::ChatFullInfo) -> Option<String> {
    if let Some(handle) = chat.username() {
        return Some(format!("@{handle}"));
    }
    let first = chat.first_name()?;
    Some(match chat.last_name() {
        Some(last) => format!("{first} {last}"),
        None => first.to_string(),
    })
}

/// Fire-and-forget usage logging; never delays request handling.
fn log_request(app: &Arc<App>, user_id: i64, kind: &'static str) {
    let store = app.core.deps.store.clone();
    tokio::spawn(async move {
        let logged = tokio::task::spawn_blocking(move || {
            let account_id = store.account_for_telegram(user_id)?;
            store.log_request(account_id, kind)
        })
        .await;
        match logged {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "request logging failed"),
            Err(e) => tracing::warn!(error = %e, "request logging join failed"),
        }
    });
}

/// Telegram clears the "typing…" indicator about five seconds after the
/// action is sent, so keeping it lit means resending it.
const TYPING_REFRESH: std::time::Duration = std::time::Duration::from_secs(4);

/// Holds Telegram's "typing…" indicator up for as long as it is alive, and
/// drops it the moment it goes out of scope.
///
/// The progress message covers the parts of a run that produce events. It
/// cannot cover the parts that produce nothing — a slow page render, or a
/// model call that stalls — and during those the last edit just sits there,
/// indistinguishable from a crashed bot. One user watched five minutes of
/// that. The indicator is the cheapest honest signal available: it says the
/// process is alive and this run has not been abandoned, without editing a
/// message or claiming progress that isn't happening.
struct Typing(tokio::task::JoinHandle<()>);

impl Typing {
    fn start(bot: Bot, chat_id: ChatId) -> Self {
        Self(tokio::spawn(async move {
            loop {
                // Failures are ignored on purpose: a missing indicator must
                // never be the reason a reply doesn't arrive.
                let _ = bot.send_chat_action(chat_id, ChatAction::Typing).await;
                tokio::time::sleep(TYPING_REFRESH).await;
            }
        }))
    }
}

impl Drop for Typing {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Fire-and-forget name refresh, so `/stat` has something to print besides
/// an id. Separate from `log_request` on purpose: running a command should
/// teach the bot your name without inflating your request count.
fn note_user(app: &Arc<App>, user_id: i64, name: String) {
    let store = app.core.deps.store.clone();
    tokio::spawn(async move {
        let noted = tokio::task::spawn_blocking(move || {
            let account_id = store.account_for_telegram(user_id)?;
            store.remember_user(account_id, &name)
        })
        .await;
        match noted {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "display name update failed"),
            Err(e) => tracing::warn!(error = %e, "display name join failed"),
        }
    });
}

fn note_sender(app: &Arc<App>, msg: &Message) {
    if let Some(user) = msg.from.as_ref() {
        note_user(app, user.id.0 as i64, display_name(user));
        note_chat(app, user.id.0 as i64, msg.chat.id.0);
    }
}

/// Records where this person is talking, so `/advert` can reach them. A
/// user id is only the same as a chat id in a private chat.
fn note_chat(app: &Arc<App>, user_id: i64, chat_id: i64) {
    let store = app.core.deps.store.clone();
    tokio::spawn(async move {
        let recorded = tokio::task::spawn_blocking(move || {
            let account_id = store.account_for_telegram(user_id)?;
            store.note_delivery(account_id, "telegram", &chat_id.to_string())
        })
        .await;
        match recorded {
            Ok(Err(e)) => tracing::warn!(error = %e, "could not record the user's chat"),
            Err(e) => tracing::warn!(error = %e, "chat recording task failed"),
            Ok(Ok(())) => {}
        }
    });
}

/// How an announcement reads when it lands. Marked as one, because it
/// arrives unprompted in a chat that is otherwise only ever a reply — and
/// says who sent it, so nobody has to wonder whether the bot has started
/// messaging people on its own.
pub fn advert_message(body: &str, from: &str) -> String {
    format!("Announcement from {from}:\n\n{}", body.trim())
}

/// What `/advert` refuses to send, with the reason as the user will read it.
pub fn check_advert(body: &str) -> Result<&str, String> {
    let body = body.trim();
    if body.is_empty() {
        return Err(
            "usage: /advert <message> — sends it to everyone who has used this bot. \
             Nothing is sent until you give it something to say."
                .to_string(),
        );
    }
    // Telegram rejects anything over 4096 characters, and the prefix has to
    // fit too. Better to say so than to have the send fail per recipient.
    if body.chars().count() > 3500 {
        return Err(format!(
            "that is {} characters; keep an announcement under 3500 so it fits in one message",
            body.chars().count()
        ));
    }
    Ok(body)
}














/// One recipient's outcome in a broadcast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivered {
    Ok,
    /// Cannot be reached again, ever: blocked the bot, deleted the account,
    /// or the chat is gone. Kept apart from `Failed` because it is
    /// permanent — a waitlist row for one of these would otherwise be
    /// retried at every future round, forever.
    Gone,
    Failed,
}

/// Telegram's bulk limit is around 30 messages per second across the whole
/// token. `/advert` was written when everyone who used this bot lived in
/// one house and a sequential loop stayed under that by accident; a hundred
/// members is where that stops being true. 20 per second leaves room for
/// the ordinary replies the bot is still sending while a broadcast runs.
const BROADCAST_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Sends `text` to each `(recipient, chat)` in turn and reports what
/// happened to each, so a caller that has bookkeeping to do — the announce
/// stamps who it reached and drops who it never can — knows which is which.
async fn broadcast(
    bot: &Bot,
    targets: &[(i64, i64)],
    text: &str,
    parse_mode: Option<ParseMode>,
) -> Vec<(i64, Delivered)> {
    let mut out = Vec::with_capacity(targets.len());
    for (i, (recipient, chat_id)) in targets.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(BROADCAST_INTERVAL).await;
        }
        let outcome = send_once(bot, ChatId(*chat_id), text, parse_mode).await;
        if outcome != Delivered::Ok {
            tracing::warn!(recipient, chat_id, ?outcome, "broadcast message not delivered");
        }
        out.push((*recipient, outcome));
    }
    out
}

async fn send_once(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    parse_mode: Option<ParseMode>,
) -> Delivered {
    let attempt = async |bot: &Bot| {
        let req = bot.send_message(chat_id, text.to_string());
        match parse_mode {
            Some(mode) => req.parse_mode(mode).await.map(|_| ()),
            None => req.await.map(|_| ()),
        }
    };
    let Err(e) = attempt(bot).await else {
        return Delivered::Ok;
    };
    // Flood control is Telegram asking the *bot* to slow down, not a
    // recipient who cannot be reached. Counting it as a failure would
    // report a delivered-later message as lost; waiting the time it named
    // is the only thing that works.
    if let teloxide::RequestError::RetryAfter(after) = &e {
        let wait = after.duration();
        if wait <= MAX_FLOOD_WAIT {
            tracing::warn!(
                seconds = wait.as_secs(),
                chat_id = chat_id.0,
                "broadcast throttled; waiting as asked"
            );
            tokio::time::sleep(wait).await;
            return match attempt(bot).await {
                Ok(()) => Delivered::Ok,
                Err(e) => classify_send(&e),
            };
        }
    }
    classify_send(&e)
}

fn classify_send(e: &teloxide::RequestError) -> Delivered {
    use teloxide::{ApiError, RequestError};
    match e {
        RequestError::Api(
            ApiError::BotBlocked | ApiError::UserDeactivated | ApiError::ChatNotFound,
        ) => Delivered::Gone,
        _ => Delivered::Failed,
    }
}







async fn blocking<T, F>(f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r)
}



/// Delivers an announcement core has planned, then tells core what landed.
///
/// Three steps on purpose: core decides who and what, the adapter sends, core
/// records. That is the shape a network forces, and it is the reason a failed
/// send is retried while a blocked recipient is dropped.
async fn announce_round(
    bot: &Bot,
    msg: &Message,
    app: &Arc<App>,
    code: &str,
) -> ResponseResult<()> {
    let store = app.core.store();
    let code_owned = code.to_string();
    let planned = match blocking(move || crate::invites::plan_announcement(&store, &code_owned)).await {
        Ok(planned) => planned,
        Err(e) => {
            tracing::error!(error = %e, "could not plan the announcement");
            bot.send_message(msg.chat.id, "Sorry, I couldn't read the rounds.").await?;
            return Ok(());
        }
    };
    let (targets, text) = match planned {
        crate::invites::Announcement::Refused(reason) => {
            bot.send_message(msg.chat.id, reason).await?;
            return Ok(());
        }
        crate::invites::Announcement::Ready { targets, text } => (targets, text),
    };

    let results = broadcast(bot, &targets, &text, Some(ParseMode::Html)).await;

    let outcomes: Vec<(i64, crate::invites::Reached)> = results
        .iter()
        .map(|(recipient, outcome)| {
            let reached = match outcome {
                Delivered::Ok => crate::invites::Reached::Yes,
                Delivered::Gone => crate::invites::Reached::Gone,
                Delivered::Failed => crate::invites::Reached::No,
            };
            (*recipient, reached)
        })
        .collect();

    let sent = outcomes.iter().filter(|(_, r)| *r == crate::invites::Reached::Yes).count();
    let dropped = outcomes.iter().filter(|(_, r)| *r == crate::invites::Reached::Gone).count();
    let retryable = outcomes.iter().filter(|(_, r)| *r == crate::invites::Reached::No).count();

    let store = app.core.store();
    if let Err(e) = blocking(move || crate::invites::record_announcement(&store, &outcomes)).await {
        tracing::warn!(error = %e, "could not record what the announcement reached");
    }

    let mut report = format!("Told {sent} of {} people about \"{code}\".", targets.len());
    if dropped > 0 {
        report.push_str(&format!(
            "\nDropped {dropped} who have blocked Scout or deleted the chat."
        ));
    }
    if retryable > 0 {
        report.push_str(&format!(
            "\n{retryable} couldn't be reached this time; run it again to retry them."
        ));
    }
    report.push_str("\n\nThe message asks them to send the join command rather \
                     than tap a link, because a link carries no code into a chat \
                     that already has history.");
    bot.send_message(msg.chat.id, report).await?;
    Ok(())
}



async fn handle_text(bot: Bot, msg: Message, app: Arc<App>) -> ResponseResult<()> {
    let text = msg.text().unwrap_or_default().to_string();
    let chat_id = msg.chat.id;
    let Some(user_id) = sender_id(&msg) else { return Ok(()) };
    // Checked before the request is logged: otherwise somebody over their
    // cap would push their own count up by being told they are over it, and
    // /stat would report refusals as work.
    if let Some(refusal) = crate::session::over_daily_cap(&app.core, user_id).await {
        bot.send_message(chat_id, refusal).await?;
        return Ok(());
    }
    log_request(&app, user_id, "text");
    note_sender(&app, &msg);

    // Session expiry: after a long gap the old context is set aside; a quick
    // LLM check restores it when the new message continues the same topic.
    // The session belongs to one (chat, user) pair, so what is restored is
    // always the caller's own context — in a group, user B can never inherit
    // user A's.
    let key = chat_key(chat_id.0, user_id);
    {
        // The draft is still in memory and still dies with the gap.
        let mut chat = app.chats.entry(key).or_default();
        take_expired_session(&mut chat, std::time::Instant::now());
    }
    let scope = crate::session::conversation_scope(chat_id.0, user_id);
    let conversation_id = match crate::session::resolve_conversation(&app.core, user_id, &scope, &text).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, chat_id = chat_id.0, user_id, "could not open a conversation");
            return Ok(());
        }
    };

    let pending = app
        .chats
        .get(&key)
        .and_then(|c| c.pending_draft.clone());
    let resolution = resolve_draft(pending.as_deref(), &text);
    let had_draft = !matches!(resolution, DraftResolution::NoDraft);
    if had_draft {
        if let Some(mut chat) = app.chats.get_mut(&key) {
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

    let prompt = if looks_like_price_request(&prompt) {
        tracing::info!(chat_id = chat_id.0, "price request; requiring compare_prices");
        format!("{prompt}{PRICE_REQUEST_NOTE}")
    } else {
        prompt
    };

    let _typing = Typing::start(bot.clone(), chat_id);

    let (events, incoming) = tokio::sync::mpsc::unbounded_channel();
    let live = Live::new(bot.clone(), chat_id, app.streams.clone());
    // join! rather than spawn: both futures run in this task, so the
    // renderer's futures need no Send bound. `events` moves into the run and
    // drops when it returns, which is what ends the renderer.
    let (result, mut live) = tokio::join!(
        run_agent(&app, events, user_id, chat_id.0, conversation_id, &prompt),
        crate::progress::render_events(live, incoming),
    );
    match result {
        Ok(reply) => deliver(&bot, &app, &mut live, chat_id, &reply).await?,
        Err(e) => {
            tracing::error!(error = %e, chat_id = chat_id.0, "agent request failed");
            // Replace the progress message rather than sending a second one:
            // otherwise the user is left with a half-written thought frozen
            // above the apology.
            live.show(agent_error_message(&e), true).await;

        }
    }
    Ok(())
}

/// Markers of a "find me the cheapest one" request, in the languages this
/// bot's users write in. Substrings, so Russian and Dutch inflections match.
const PRICE_MARKERS: &[&str] = &[
    "cheapest",
    "cheaper",
    "best price",
    "lowest price",
    "best deal",
    "bargain",
    "дешев",   // дешевый / дешевле / подешевле
    "дешёв",
    "лучшая цена",
    "лучшую цену",
    "goedkoop", // goedkoop / goedkoopste / goedkoper
    "beste prijs",
];

/// Appended to the user's message when they ask for the cheapest option.
/// The preamble carries the same rule, but it competes with a dozen others
/// halfway through a long search — as the freshest line in the prompt it
/// actually fires. (Observed: a cheapest-price request that opened eight
/// pages and answered from raw snippets without comparing anything.)
const PRICE_REQUEST_NOTE: &str = "\n\n[system note] This is a cheapest-price request. \
Before answering you MUST call compare_prices once, passing every candidate offer with its \
price, currency, pack size (units) and shipping cost where stated. Present its best_single \
and best_per_unit, using its numbers verbatim. Keep enough turns free to do it: search \
first, open at most 3 pages, then compare.";

fn looks_like_price_request(text: &str) -> bool {
    let text = text.to_lowercase();
    PRICE_MARKERS.iter().any(|m| text.contains(m))
}

/// How much of the model's own notes to hand the wrap-up agent.
const WRAP_UP_CONTEXT: usize = 6000;
/// No stream item for this long means the run is stuck. Generous, because a
/// tool call (three site searches, a page fetch and its dead-link probes)
/// runs between items — but all of those carry their own timeouts well
/// under this. rig's client has no timeout of its own, so this is the only
/// thing standing between a stalled connection and a chat that waits
/// forever.
const STREAM_STALL: std::time::Duration = std::time::Duration::from_secs(90);
/// Hard ceiling on one request. A thorough price comparison takes ~60-90s;
/// past this the user is better served by an answer built from the notes.
const RUN_BUDGET: std::time::Duration = std::time::Duration::from_secs(300);
/// The salvage write-up is one tool-less call.
const WRAP_UP_BUDGET: std::time::Duration = std::time::Duration::from_secs(90);

/// The last `max` characters — the newest notes are the ones carrying
/// confirmed prices.
fn tail_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(max)).collect()
}

/// rig wraps the turn-limit failure in its own error types; the message is
/// the stable part across them.
fn is_max_turns(e: &impl std::fmt::Display) -> bool {
    let text = e.to_string();
    text.contains("MaxTurnsError") || text.contains("max turns")
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
    let Some(user_id) = sender_id(&msg) else { return Ok(()) };
    // Before the session is touched as well as before the request is
    // logged: a refused photo should leave the conversation exactly as it
    // was, not clear a history and disarm a draft on its way out.
    if let Some(refusal) = crate::session::over_daily_cap(&app.core, user_id).await {
        bot.send_message(chat_id, refusal).await?;
        return Ok(());
    }
    let key = chat_key(chat_id.0, user_id);
    {
        // A photo after a long gap always starts fresh (no continuation
        // check — a new photo is a new product hunt), and any stale draft
        // is dropped either way.
        let mut chat = app.chats.entry(key).or_default();
        chat.last_seen = Some(std::time::Instant::now());
        chat.pending_draft = None;
    }
    // Sizes are ordered smallest to largest; take the largest.
    let Some(photo) = msg.photo().and_then(|sizes| sizes.last()) else {
        return Ok(());
    };
    log_request(&app, user_id, "photo");
    note_sender(&app, &msg);
    // Download plus a vision call, with no progress message in this path —
    // the indicator is the only sign anything is happening.
    let _typing = Typing::start(bot.clone(), chat_id);

    let bytes = match download_photo(&bot, photo.file.id.clone()).await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!(error = %e, chat_id = chat_id.0, user_id, "photo download failed");
            bot.send_message(
                chat_id,
                "Sorry, I couldn't download that photo. Please try again.",
            )
            .await?;
            return Ok(());
        }
    };

    match describe_photo(&app.core.deps.llm, &bytes, msg.caption()).await {
        Ok(draft) => {
            app.chats.entry(key).or_default().pending_draft = Some(draft.clone());
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
            tracing::error!(error = %e, chat_id = chat_id.0, user_id, "photo description failed");
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
    // The same gate the message branches use. Reactions are routed on their
    // own branch, so an allowlist check left behind here would have dropped
    // every invited member's 👍 while their messages worked fine.
    if !is_member_id(&app, user_id) {
        return Ok(());
    }
    if !thumbs_up_added(&reaction.old_reaction, &reaction.new_reaction) {
        tracing::debug!("not a newly added thumbs-up; ignoring");
        return Ok(());
    }
    let chat_id = reaction.chat.id;
    let Some(text) = chat_reply_text(&app.replies, chat_id.0, reaction.message_id.0) else {
        // Reacted to something we no longer (or never) tracked — stay quiet
        // toward the user (the cache is in-memory, so replies from before the
        // last restart can't be resolved).
        tracing::info!(
            message_id = reaction.message_id.0,
            "thumbs-up on an untracked message (sent before last restart?); ignoring"
        );
        return Ok(());
    };

    log_request(&app, user_id, "reaction");
    note_user(&app, user_id, display_name(user));
    let _typing = Typing::start(bot.clone(), chat_id);
    let prompt = format!(
        "[system note] The user reacted with a thumbs-up to this earlier reply \
         of yours:\n---\n{text}\n---\nThat means they are considering buying \
         one of the products in it. If the reply contains exactly one product, \
         ask them to confirm saving it to purchase memory (confirm store and \
         price while you're at it). If it contains several, list them as a \
         short numbered list and ask which one to save. Do NOT call \
         record_purchase until they confirm."
    );
    // A reaction continues whatever thread this chat is already in, so it
    // never starts one: an empty excerpt would just make the classifier
    // guess.
    let scope = crate::session::conversation_scope(chat_id.0, user_id);
    let conversation_id = match crate::session::resolve_conversation(&app.core, user_id, &scope, &prompt).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, chat_id = chat_id.0, "could not open a conversation");
            return Ok(());
        }
    };
    let (events, incoming) = tokio::sync::mpsc::unbounded_channel();
    let live = Live::new(bot.clone(), chat_id, app.streams.clone());
    let (result, mut live) = tokio::join!(
        run_agent(&app, events, user_id, chat_id.0, conversation_id, &prompt),
        crate::progress::render_events(live, incoming),
    );
    match result {
        Ok(reply) => deliver(&bot, &app, &mut live, chat_id, &reply).await?,
        Err(e) => {
            tracing::error!(error = %e, chat_id = chat_id.0, "reaction follow-up failed");
            live.show(agent_error_message(&e), true).await;
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
///
/// The run reports progress as events rather than drawing them, because the
/// tool calls alone take most of a minute and an idle chat looks broken —
/// but who draws them is not this function's business.
///
/// `events` is taken by value: returning drops it, which closes the channel
/// and ends whoever is rendering. That is the only shutdown signal the
/// renderer gets, so it must not be held anywhere else.
async fn run_agent(
    app: &App,
    events: crate::events::EventSink,
    user_id: i64,
    chat_id: i64,
    conversation_id: i64,
    prompt: &str,
) -> anyhow::Result<String> {
    let account_id = crate::session::account_of(&app.core, user_id).await?;
    let facts = {
        let store = app.core.deps.store.clone();
        tokio::task::spawn_blocking(move || store.list_facts(account_id)).await??
    };
    let agent = build_agent(&app.core.deps, account_id, chat_id, &facts);
    // History comes from the conversation the caller opened, so an
    // in-flight run always reads and writes that thread and never anyone
    // else's — the isolation the (chat, user) map used to provide.
    let mut history = {
        let store = app.core.deps.store.clone();
        blocking(move || crate::session::load_history(&store, conversation_id, HISTORY_CAP)).await?
    };

    let mut streamed = String::new();
    // Reasoning arrives on its own channel, separate from the answer text.
    let mut thinking = String::new();
    let mut final_response = None;
    // The whole streamed run sits inside one deadline. A guard on
    // stream.next() alone is not enough: it leaves every await in the loop
    // body uncovered, and the budget check below it can only fire when an
    // item arrives — so a run that stops receiving anything is bounded by
    // nothing. Observed once at fifteen minutes before the provider finally
    // errored, with the user watching a frozen progress message.
    let outcome: Result<Result<Option<&'static str>, anyhow::Error>, _> =
        tokio::time::timeout(RUN_BUDGET, async {
            let mut stream = agent.stream_chat(prompt, history.clone()).await;
            loop {
                // A silent stream is a stall even while the run as a whole
                // still has time left.
                let next = match tokio::time::timeout(STREAM_STALL, stream.next()).await {
                    Ok(Some(item)) => item,
                    Ok(None) => return Ok(None),
                    Err(_) => return Ok(Some("the model stopped responding")),
                };
                let item = match next {
                    Ok(item) => item,
                    // Not fatal: by this point the research is usually done
                    // and only the write-up is missing. Salvaged below.
                    Err(e) if is_max_turns(&e) => {
                        return Ok(Some("I ran out of research steps"))
                    }
                    Err(e) => return Err(anyhow::Error::from(e)),
                };
                match item {
                MultiTurnStreamItem::ToolExecutionStart { tool_call, .. } => {
                    let args = &tool_call.function.arguments;
                    crate::events::emit(
                        &events,
                        crate::events::AgentEvent::Tool(crate::progress::describe(
                            &tool_call.function.name,
                            args,
                        )),
                    );
                }
                MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(t)) => {
                    streamed.push_str(&t.text);
                    // Unclosed <think> blocks render as nothing, so inline
                    // reasoning never reaches the chat as answer text.
                    let answer = strip_thinking(&streamed);
                    if !answer.is_empty() {
                        crate::events::emit(&events, crate::events::AgentEvent::Answer(answer));
                    }
                }
                // MiniMax streams its reasoning on a separate channel. Shown
                // in italics while it works, replaced by the answer after.
                MultiTurnStreamItem::StreamAssistantItem(
                    StreamedAssistantContent::ReasoningDelta { reasoning, .. },
                ) => {
                    thinking.push_str(&reasoning);
                    if strip_thinking(&streamed).is_empty() {
                        crate::events::emit(
                            &events,
                            crate::events::AgentEvent::Thinking(thinking.clone()),
                        );
                    }
                }
                MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Reasoning(
                    r,
                )) => {
                    for block in &r.content {
                        if let rig::completion::message::ReasoningContent::Text { text, .. } = block
                        {
                            thinking.push_str(text);
                        }
                    }
                    if strip_thinking(&streamed).is_empty() {
                        crate::events::emit(
                            &events,
                            crate::events::AgentEvent::Thinking(thinking.clone()),
                        );
                    }
                }
                MultiTurnStreamItem::FinalResponse(res) => {
                    final_response = Some(res);
                    return Ok(None);
                }
                _ => {}
            }
            }
        })
        .await;

    // Partial text survives a dropped future: whatever the run wrote before
    // the deadline is still in `streamed`/`thinking` for the wrap-up.
    let salvage = match outcome {
        Ok(Ok(reason)) => reason,
        Ok(Err(e)) => return Err(e),
        Err(_) => Some("this took too long"),
    };

    // The streamed deltas are the answer; the final response is the
    // authority on both the text and the history to keep.
    let (text, new_history) = match final_response {
        Some(res) => (res.output().to_string(), res.messages().map(|m| m.to_vec())),
        None => (streamed.clone(), None),
    };
    if let Some(h) = new_history {
        history = h;
    }
    let mut reply = strip_thinking(&text);
    if reply.is_empty() {
        reply = strip_thinking(&streamed);
    }

    if let Some(reason) = salvage {
        tracing::warn!(chat_id, reason, "run interrupted; writing up from notes");
        crate::events::emit(
            &events,
            crate::events::AgentEvent::Notice(
                "✍️ wrapping up with what I found so far".to_string(),
            ),
        );
        // The history of an interrupted run is never returned, so the
        // model's own notes are the material: its reasoning already lists
        // the prices it confirmed.
        let notes = tail_chars(&format!("{thinking}\n\n{streamed}"), WRAP_UP_CONTEXT);
        let wrap_up = wrap_up_agent(&app.core.deps, &facts);
        let asked = format!(
            "{WRAP_UP_NOTE}\nTell the user briefly that {reason}, then give the answer.\n\n\
             Your research notes so far:\n{notes}"
        );
        // The salvage attempt gets its own deadline: it must never become a
        // second way to hang.
        reply = match tokio::time::timeout(
            WRAP_UP_BUDGET,
            rig::completion::Prompt::prompt(&wrap_up, asked),
        )
        .await
        {
            Ok(Ok(text)) => strip_thinking(&text),
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => anyhow::bail!("wrap-up timed out after the run was interrupted"),
        };
        // Keep the exchange in context; the interrupted turns are lost.
        history.push(LlmMessage::user(prompt));
        history.push(LlmMessage::assistant(&reply));
    }

    // Guard against links the model wrote but never saw: an invented Amazon
    // /dp/<ASIN> URL reads as a real product page and answers 404. One repair
    // turn, then scrub whatever is still dead.
    let dead = crate::links::dead_links_in(&app.core.deps.http, &reply).await;
    if !dead.is_empty() {
        tracing::warn!(?dead, chat_id, "dead links in reply; asking the agent to correct it");
        let note = crate::links::repair_prompt(&dead);
        reply = strip_thinking(&agent.chat(note, &mut history).await?);
        let still_dead = crate::links::dead_links_in(&app.core.deps.http, &reply).await;
        if !still_dead.is_empty() {
            tracing::warn!(?still_dead, chat_id, "dead links survived the correction; stripping");
            reply = crate::links::strike_dead(&reply, &still_dead);
        }
    }

    trim_history(&mut history, HISTORY_CAP);
    let store = app.core.deps.store.clone();
    if let Err(e) = blocking(move || crate::session::save_history(&store, conversation_id, &history)).await {
        // The answer is already on its way to the user; losing the thread is
        // worse than not saving it, but it is not worth failing the reply.
        tracing::warn!(error = %e, conversation_id, "could not save the conversation");
    }
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
    let full = std::mem::take(history);
    let mut window = full[full.len() - cap..].to_vec();
    match window.iter().position(is_plain_user_text) {
        Some(0) => *history = window,
        Some(i) => {
            window.drain(..i);
            *history = window;
        }
        // One turn produced more tool traffic than the whole cap, so the
        // window is tool calls all the way up and there is no safe head in
        // it. Clearing here is what turned a four-leg trip search into "I
        // don't have a recent flight search in our conversation", with the
        // search still on screen above the reply.
        //
        // Keep the prose instead. Text carries no call/result pairing, so
        // it cannot be orphaned however it is cut, and it is the part worth
        // remembering — what was asked and what was answered.
        None => {
            let mut text: Vec<LlmMessage> = full.into_iter().filter(is_text_only).collect();
            if text.len() > cap {
                text.drain(..text.len() - cap);
            }
            *history = text;
        }
    }
}

/// A message that is prose and nothing else.
///
/// Providers reject a tool result whose call was trimmed away; text has no
/// such pairing to break, so a history of text alone is safe to send no
/// matter where it was cut.
fn is_text_only(msg: &LlmMessage) -> bool {
    match msg {
        LlmMessage::User { content } => {
            content.iter().all(|c| matches!(c, rig::message::UserContent::Text(_)))
        }
        LlmMessage::Assistant { content, .. } => {
            content.iter().all(|c| matches!(c, rig::message::AssistantContent::Text(_)))
        }
        // Instruction text, and never part of a call/result pair.
        LlmMessage::System { .. } => true,
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

/// Puts the finished answer where the progress message already is: the first
/// chunk replaces it, any remainder follows as new messages. Every chunk is
/// remembered so a later 👍 resolves to its text.
async fn deliver(
    bot: &Bot,
    app: &App,
    live: &mut Live,
    chat_id: ChatId,
    text: &str,
) -> ResponseResult<()> {
    let mut chunks = split_message(text, TELEGRAM_LIMIT).into_iter();
    let Some(first) = chunks.next() else {
        live.show("(no answer - please try again)", true).await;
        return Ok(());
    };
    // The answer goes into the progress message when it can. When it
    // cannot — flood control, most often, after a long run has been editing
    // the same message for minutes — `Live` swallows the failure, so
    // without this the finished answer would vanish and the chat would keep
    // whatever half-written frame was on screen. Sending it instead goes
    // through the path that waits as long as Telegram asked.
    if live.show(&first, true).await {
        if let Some(id) = live.message_id() {
            remember_chat_reply(&app.replies, chat_id.0, id.0, live.shown());
        }
    } else {
        tracing::warn!(chat_id = chat_id.0, "could not edit the answer in; sending it instead");
        send_chunked(bot, app, chat_id, &first).await?;
    }
    send_chunked(bot, app, chat_id, &chunks.collect::<Vec<_>>().join("\n")).await
}

/// A guess at how long to wait before trying a failed send again.
const RETRY_PAUSE: std::time::Duration = std::time::Duration::from_millis(500);

/// The longest this will sit waiting on Telegram's flood control.
///
/// A deploy gives handlers 330s to drain, so a wait longer than this would
/// hold one open past its welcome. Beyond it the answer is given up on
/// rather than blocking a shutdown.
const MAX_FLOOD_WAIT: std::time::Duration = std::time::Duration::from_secs(240);

/// How long to wait before retrying a send, given why it failed.
///
/// `None` means do not retry — the wait Telegram asked for is longer than
/// this is willing to hold a handler open.
///
/// Measured in production: a long research run drew `RetryAfter(238s)` on
/// one chat. The old code paused a flat 500ms and tried again, which cannot
/// possibly satisfy a 238-second penalty, so the second attempt failed too
/// and `?` threw away a finished answer the user never saw. When Telegram
/// names a number, that number is the only one that works.
fn retry_delay(error: &teloxide::RequestError) -> Option<std::time::Duration> {
    match error {
        teloxide::RequestError::RetryAfter(after) => {
            let wait = after.duration();
            (wait <= MAX_FLOOD_WAIT).then_some(wait)
        }
        _ => Some(RETRY_PAUSE),
    }
}

/// Send in <=4096-char chunks; each chunk gets one retry. Sent chunks are
/// remembered per chat so a later reaction on one can be resolved to its
/// text.
async fn send_chunked(bot: &Bot, app: &App, chat_id: ChatId, text: &str) -> ResponseResult<()> {
    let chunks = split_message(text, TELEGRAM_LIMIT);
    for chunk in chunks {
        let sent = match bot.send_message(chat_id, chunk.clone()).await {
            Ok(sent) => sent,
            Err(e) => {
                let Some(wait) = retry_delay(&e) else {
                    tracing::error!(error = %e, "flood wait too long to hold the answer for");
                    return Err(e);
                };
                tracing::warn!(error = %e, seconds = wait.as_secs(), "send failed; waiting as asked");
                tokio::time::sleep(wait).await;
                bot.send_message(chat_id, chunk.clone()).await?
            }
        };
        remember_chat_reply(&app.replies, chat_id.0, sent.id.0, &chunk);
    }
    Ok(())
}

/// Parses a Telegram invite command and hands the request to core.
///
/// Parsing the wire format is the adapter's job; deciding what the request
/// means is not. Announce is routed separately because it has to *send*
/// things, which core cannot do.
async fn handle_invite(bot: &Bot, msg: &Message, app: &Arc<App>, arg: &str) -> ResponseResult<()> {
    let Some(user_id) = sender_id(msg) else { return Ok(()) };
    // Checked here as well as inside core: announce does not go through
    // `invite`, so relying on core's check alone would leave it open.
    if !app.core.is_admin(user_id) {
        bot.send_message(msg.chat.id, crate::invites::NOT_ADMIN).await?;
        return Ok(());
    }
    let cmd = match crate::invites::parse_invite(arg) {
        Ok(cmd) => cmd,
        Err(problem) => {
            bot.send_message(msg.chat.id, problem).await?;
            return Ok(());
        }
    };
    if let crate::invites::InviteCmd::Announce(code) = &cmd {
        return announce_round(bot, msg, app, code).await;
    }
    // Only a new round needs the link, and losing the round because a
    // username lookup blipped would be worse than a reply without one.
    let username = match cmd {
        crate::invites::InviteCmd::New { .. } => {
            match teloxide::prelude::Requester::get_me(bot).await {
                Ok(me) => me.username.clone(),
                Err(e) => {
                    tracing::warn!(error = %e, "could not read the bot's username");
                    None
                }
            }
        }
        _ => None,
    };
    let reply = crate::invites::invite(&app.core, user_id, cmd, username.as_deref()).await;
    bot.send_message(msg.chat.id, reply).await?;
    Ok(())
}

/// Hands a moderation request to core, then follows the answer with the
/// gate's cache.
async fn handle_kick(
    bot: &Bot,
    msg: &Message,
    app: &Arc<App>,
    arg: &str,
    kicking: bool,
) -> ResponseResult<()> {
    let Some(user_id) = sender_id(msg) else { return Ok(()) };
    let outcome = crate::invites::kick(&app.core, user_id, arg, kicking).await;
    // The table is the record; the set follows it.
    if let (Some(now_member), Ok(target)) = (outcome.membership, arg.trim().parse::<i64>()) {
        if now_member {
            app.members.insert(target);
        } else {
            app.members.remove(&target);
        }
    }
    bot.send_message(msg.chat.id, outcome.reply).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn history_survives_being_dropped_and_reloaded() {
        let (s, _d) = crate::store::tests::test_store();
        let a = s.account_for_telegram(11).unwrap();
        let c = s.start_conversation(a, "direct").unwrap();

        let original = vec![LlmMessage::user("cheapest beans"), LlmMessage::assistant("here")];
        crate::session::save_history(&s, c, &original).unwrap();
        let loaded = crate::session::load_history(&s, c, HISTORY_CAP).unwrap();

        // Not compared struct-for-struct: rig leaves `additional_params`
        // as None when a message is built in code but deserializes it to
        // Some({}), so a round trip is never byte-identical. What has to
        // hold is that the words and their roles survive, and that a second
        // trip changes nothing further — otherwise history would drift a
        // little every time it was reloaded.
        assert_eq!(loaded.len(), original.len());
        assert_eq!(crate::session::last_messages_text(&loaded, 2), crate::session::last_messages_text(&original, 2));

        crate::session::save_history(&s, c, &loaded).unwrap();
        let again = crate::session::load_history(&s, c, HISTORY_CAP).unwrap();
        assert_eq!(again, loaded, "reloading must reach a fixed point");
    }

    #[test]
    fn saving_replaces_rather_than_appends() {
        let (s, _d) = crate::store::tests::test_store();
        let a = s.account_for_telegram(11).unwrap();
        let c = s.start_conversation(a, "direct").unwrap();

        crate::session::save_history(&s, c, &[LlmMessage::user("one"), LlmMessage::assistant("two")]).unwrap();
        // trim_history can drop from the front; the store must follow it
        // down rather than keeping the messages the agent will never see.
        crate::session::save_history(&s, c, &[LlmMessage::assistant("two")]).unwrap();

        let loaded = crate::session::load_history(&s, c, HISTORY_CAP).unwrap();
        assert_eq!(loaded.len(), 1, "a trimmed history must not grow back");
    }


    use super::{thumbs_up_added, ChatSession, SENT_REPLY_CAP};
    use dashmap::DashMap;
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
    fn the_typing_indicator_is_refreshed_before_telegram_drops_it() {
        // Telegram clears the action after about five seconds. Refreshing on
        // or after that boundary leaves visible gaps, and a gap in the only
        // liveness signal we have is exactly the thing this replaced.
        use super::TYPING_REFRESH;
        assert!(
            TYPING_REFRESH < std::time::Duration::from_secs(5),
            "refresh must beat Telegram's ~5s expiry, got {TYPING_REFRESH:?}"
        );
    }

    #[test]
    fn session_expiry_boundary() {
        use super::session_expired;
        use crate::session::SESSION_TTL;
        use std::time::Instant;
        let now = Instant::now();
        assert!(!session_expired(None, now), "first contact is never stale");
        assert!(!session_expired(Some(now - crate::session::SESSION_TTL / 2), now));
        assert!(session_expired(
            Some(now - crate::session::SESSION_TTL - std::time::Duration::from_secs(1)),
            now
        ));
    }


    #[test]
    fn chat_key_isolates_session_state_per_user() {
        // The previous DashMap<i64, ChatSession> collapsed every allowed
        // sender in the same chat into one entry. The new key tuple forces
        // each user to a distinct slot, so their history and draft never
        // cross.
        let chats: DashMap<(i64, i64), ChatSession> = DashMap::new();

        let alice_key = chat_key(9001, 111);
        let bob_key = chat_key(9001, 222);

        // Alice arms a draft. History no longer lives here — it is scoped
        // per (account, conversation scope) in the store — but the draft
        // still is, and it must stay hers.
        {
            let mut s = chats.entry(alice_key).or_default();
            s.pending_draft = Some("USB hub".to_string());
        }

        // Bob's entry is independent — same chat, different user.
        assert!(chats.get(&bob_key).is_none());
        chats.entry(bob_key).or_default().last_seen = Some(std::time::Instant::now());

        // Guards are scoped: DashMap deadlocks against itself if a read
        // guard is still alive when the same shard is written below.
        {
            let alice = chats.get(&alice_key).unwrap();
            assert_eq!(alice.pending_draft.as_deref(), Some("USB hub"));

            let bob = chats.get(&bob_key).unwrap();
            assert_eq!(bob.pending_draft, None, "Alice's draft must not reach Bob");
        }

        // /reset drops one user's slot and leaves the others alone.
        chats.remove(&alice_key);
        assert!(chats.get(&alice_key).is_none());
        assert!(chats.get(&bob_key).is_some(), "removing one user must not affect others");
    }

    #[test]
    fn expiry_takes_the_history_and_disarms_the_draft() {
        use super::take_expired_session;
        use std::time::{Duration, Instant};

        let now = Instant::now();
        let stale = now - crate::session::SESSION_TTL - Duration::from_secs(1);

        // Aged out with history: hand it back for the continuation check,
        // and drop the draft — a photo drafted before the gap must not be
        // confirmed by the next bare "ok".
        // Aged out: report it, and drop the draft — a photo drafted before
        // the gap must not be confirmed by the next bare "ok".
        let mut chat = ChatSession {
            pending_draft: Some("USB hub, 4-port, black".to_string()),
            last_seen: Some(stale),
        };
        assert!(take_expired_session(&mut chat, now), "the gap should be reported");
        assert_eq!(chat.pending_draft, None, "stale draft must be disarmed");
        assert_eq!(chat.last_seen, Some(now));

        // Still inside the TTL: nothing is touched.
        let mut chat = ChatSession {
            pending_draft: Some("USB hub, 4-port, black".to_string()),
            last_seen: Some(now - crate::session::SESSION_TTL / 2),
        };
        assert!(!take_expired_session(&mut chat, now));
        assert_eq!(chat.pending_draft.as_deref(), Some("USB hub, 4-port, black"));
    }

    #[test]
    fn reply_ring_buffer_is_shared_per_chat_not_per_user() {
        // Bot replies are public; their lookup must work for any reactor in
        // the chat. This is the deliberate exception to per-user scoping.
        let replies: DashMap<i64, std::collections::VecDeque<(i32, String)>> = DashMap::new();
        remember_chat_reply(&replies, 9001, 42, "Option A: bol.com — 12.99 EUR");
        // Anyone asking about message 42 in chat 9001 sees the same text.
        assert_eq!(
            chat_reply_text(&replies, 9001, 42),
            Some("Option A: bol.com — 12.99 EUR".to_string())
        );
        // A different chat does not see it.
        assert!(chat_reply_text(&replies, 9002, 42).is_none());
        // An unknown message id in the same chat does not see it.
        assert!(chat_reply_text(&replies, 9001, 99).is_none());
    }

    #[test]
    fn reply_ring_buffer_caps() {
        let replies: DashMap<i64, std::collections::VecDeque<(i32, String)>> = DashMap::new();
        for i in 0..(SENT_REPLY_CAP as i32 + 5) {
            remember_chat_reply(&replies, 9001, i, &format!("reply {i}"));
        }
        assert_eq!(
            chat_reply_text(&replies, 9001, 0),
            None,
            "oldest evicted past SENT_REPLY_CAP"
        );
        assert!(chat_reply_text(&replies, 9001, SENT_REPLY_CAP as i32 - 1).is_some());
    }

    use super::*;

    #[test]
    fn an_advert_says_it_is_one_and_who_sent_it() {
        // It lands unprompted in a chat that is otherwise only ever a
        // reply, so it has to be obvious that a person sent it and the bot
        // has not started messaging people on its own.
        let out = advert_message("  Flights are live now.  ", "@watchcat");
        assert!(out.starts_with("Announcement from @watchcat:"), "got: {out}");
        assert!(out.contains("Flights are live now."));
        assert!(!out.contains("  Flights"), "the body should be trimmed: {out}");
    }

    #[test]
    fn an_advert_with_nothing_to_say_is_refused_before_anyone_is_messaged() {
        // Sending a bare "/advert" to the whole household would be
        // unrecallable and meaningless.
        for empty in ["", "   ", "\n"] {
            let problem = check_advert(empty).unwrap_err();
            assert!(problem.contains("usage:"), "got: {problem}");
        }
        assert_eq!(check_advert("  hello  ").unwrap(), "hello");
    }

    #[test]
    fn an_advert_too_long_for_telegram_is_refused_rather_than_failing_per_recipient() {
        let long = "x".repeat(3501);
        let problem = check_advert(&long).unwrap_err();
        assert!(problem.contains("3500"), "got: {problem}");
        assert!(check_advert(&"x".repeat(3500)).is_ok());
    }
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
    fn the_turn_limit_is_recognised_however_rig_wraps_it() {
        // Exactly what production logged when a user was left hanging.
        assert!(is_max_turns(&"PromptError: MaxTurnsError: reached max turns limit: 12"));
        assert!(is_max_turns(&"reached max turns limit: 16"));
        assert!(!is_max_turns(&"kagi api error (status 401): bad key"));
        assert!(!is_max_turns(&"perplexity request failed: timed out"));
    }

    #[test]
    fn wrap_up_notes_keep_the_newest_findings() {
        let notes = format!("{}\nconfirmed: 44.99 EUR for 9 kg", "old chatter ".repeat(500));
        let kept = tail_chars(&notes, 60);
        assert_eq!(kept.chars().count(), 60);
        assert!(kept.ends_with("confirmed: 44.99 EUR for 9 kg"), "got: {kept}");
        // shorter than the cap: untouched
        assert_eq!(tail_chars("short", 60), "short");
    }

    #[test]
    fn price_requests_are_recognised_across_languages() {
        for asking in [
            "find the cheapest ariel professional detergent",
            "Найди самый дешевый Ariel Professional liquid colour detergent",
            "где подешевле?",
            "wat is de goedkoopste optie?",
            "which one has the BEST PRICE",
        ] {
            assert!(looks_like_price_request(asking), "missed: {asking}");
        }
        for other in [
            "find me a body groomer for sensitive skin",
            "did I buy coffee last month?",
        ] {
            assert!(!looks_like_price_request(other), "false positive: {other}");
        }
    }

    #[test]
    fn a_flood_wait_is_honoured_for_as_long_as_telegram_asked() {
        use teloxide::types::Seconds;
        use teloxide::RequestError;

        // The measured case: Telegram answered RetryAfter(238s) and the old
        // code slept 500ms, so the retry could not succeed and the finished
        // answer was thrown away with the error.
        let flood = RequestError::RetryAfter(Seconds::from_seconds(238));
        assert_eq!(
            retry_delay(&flood),
            Some(std::time::Duration::from_secs(238)),
            "wait what it asked for, not a guess"
        );

        // Long enough to outlast a deploy's drain window is not worth
        // holding a handler open for.
        let forever = RequestError::RetryAfter(Seconds::from_seconds(3600));
        assert_eq!(retry_delay(&forever), None);

        // Anything else keeps the short retry it always had.
        let other = RequestError::InvalidJson {
            source: std::sync::Arc::new(
                serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
            ),
            raw: String::new().into(),
        };
        assert_eq!(retry_delay(&other), Some(RETRY_PAUSE));
    }

    #[test]
    fn no_safe_head_after_drain_falls_back_to_the_text_of_the_conversation() {
        // Not empty: an answer with nothing to anchor it is still worth
        // keeping, and dropping it is how a reply becomes "I don't have a
        // recent search in our conversation".
        let mut history = vec![
            assistant_tool_call("call-1", "search"),
            tool_result("call-1"),
            assistant_text("final answer"),
        ];
        trim_history(&mut history, 1);
        assert_eq!(history.len(), 1);
        assert!(matches!(&history[0], LlmMessage::Assistant { .. }));
    }

    #[test]
    fn one_turn_of_heavy_tool_use_does_not_erase_the_conversation() {
        // Measured in production: a four-leg trip search ran twelve searches
        // in one turn. Twenty-five messages of tool traffic pushed the
        // user's own message out of the capped window, no plain user text
        // was left to start from, and the whole history was cleared. The
        // next reply was "I don't have a recent flight search in our
        // conversation" — with the search still on screen above it.
        let mut history = vec![
            user_text("flights to Japan via Hong Kong"),
            assistant_text("let me look"),
            user_text("around 15 September"),
        ];
        for i in 0..12 {
            history.push(assistant_tool_call(&format!("call-{i}"), "search_flights"));
            history.push(tool_result(&format!("call-{i}")));
        }
        history.push(assistant_text("AMS to HKG on Etihad, EUR 369.94"));

        trim_history(&mut history, HISTORY_CAP);

        assert!(!history.is_empty(), "a turn must not be able to erase the conversation");
        assert!(
            is_plain_user_text(&history[0]),
            "whatever survives still has to start somewhere a provider accepts"
        );
        // The substance survives even though the tool traffic does not.
        let text = format!("{history:?}");
        assert!(text.contains("Hong Kong"), "the question is still there: {text}");
        assert!(text.contains("369.94"), "and so is the answer: {text}");
        assert!(!text.contains("ToolCall"), "but the tool calls are gone");
        assert!(!text.contains("ToolResult"), "and so are their results");
    }

    #[test]
    fn start_is_recognised_across_every_form_telegram_sends() {
        // A bare start and a start with a payload, each with and without
        // the @bot suffix a group chat adds.
        assert_eq!(start_payload("/start"), Some(""));
        assert_eq!(start_payload("/start@scout_bot"), Some(""));
        assert_eq!(start_payload("/start autumn-drop"), Some("autumn-drop"));
        assert_eq!(start_payload("/start@scout_bot autumn-drop"), Some("autumn-drop"));
        // Telegram clients are tidy but people are not.
        assert_eq!(start_payload("  /start   autumn-drop  "), Some("autumn-drop"));

        // A message that merely begins with the word is not the command.
        assert_eq!(start_payload("start"), None);
        assert_eq!(start_payload("started looking for a bike"), None);
        assert_eq!(start_payload("/started"), None);
        assert_eq!(start_payload("please /start"), None);
        assert_eq!(start_payload(""), None);
    }

    #[test]
    fn join_code_is_the_payload_and_nothing_else() {
        assert_eq!(join_code("/start autumn-drop"), Some("autumn-drop"));
        assert_eq!(join_code("/start@scout_bot autumn-drop"), Some("autumn-drop"));
        // A bare start carries no code — that is the "invite-only" case,
        // not a claim against a round named "".
        assert_eq!(join_code("/start"), None);
        assert_eq!(join_code("/start   "), None);
        assert_eq!(join_code("find me a bike"), None);
    }






    #[test]
    fn a_stranger_is_told_the_route_that_still_works_for_them() {
        // Anyone reading this reply has just messaged Scout, so their chat
        // has history and Telegram will never show them the START button
        // again — a link would open a chat carrying no code. Telling them
        // to go and tap one would be advice that cannot work.
        assert!(INVITE_ONLY.contains("/start "), "got: {INVITE_ONLY}");
        assert!(
            !INVITE_ONLY.to_lowercase().contains("press start"),
            "the button they are being pointed at will not appear: {INVITE_ONLY}"
        );
    }



    #[test]
    fn broadcast_pacing_stays_under_telegrams_bulk_limit() {
        // ~30 messages per second across the whole token. The delay is what
        // keeps a hundred-member announce from finding that ceiling.
        let per_second = 1000.0 / BROADCAST_INTERVAL.as_millis() as f64;
        assert!(per_second <= 30.0, "{per_second} messages/second is over the limit");
    }

    #[test]
    fn a_blocked_recipient_is_told_apart_from_a_failed_send() {
        use teloxide::{ApiError, RequestError};
        // Permanent: carrying these forward would retry the same failure at
        // every future round, forever.
        for gone in [ApiError::BotBlocked, ApiError::UserDeactivated, ApiError::ChatNotFound] {
            assert_eq!(classify_send(&RequestError::Api(gone)), Delivered::Gone);
        }
        // Transient: leave the waitlist row alone so a re-run retries it.
        assert_eq!(
            classify_send(&RequestError::Api(ApiError::Unknown("server oops".into()))),
            Delivered::Failed
        );
    }

    #[test]
    fn start_is_not_a_command_the_router_can_reach() {
        // /start has to answer people the gate rejects, so it lives on its
        // own branch. A Start variant left in the enum would be routed
        // behind the gate, and a stranger's /start would fall through to
        // silence — or a member's would reach the LLM.
        assert!(
            <Command as BotCommands>::bot_commands()
                .iter()
                .all(|c| c.command != "/start"),
            "/start must not be a gated command"
        );
    }
}
