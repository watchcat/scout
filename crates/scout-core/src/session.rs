use crate::agent::HISTORY_CAP;
use crate::core::Core;
use rig::completion::Message as LlmMessage;

/// A conversation is set aside after this long without a word, and a quick
/// LLM check decides whether the next message resumes it.
pub const SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// In a 1:1 chat Telegram makes the chat id equal the user id, and that is
/// the thread the web app will share. Anywhere else is a room with other
/// people in it and keeps its own history.
pub fn conversation_scope(chat_id: i64, user_id: i64) -> String {
    if chat_id == user_id {
        "direct".to_string()
    } else {
        format!("telegram:{chat_id}")
    }
}

/// The account behind a Telegram user, created on first sight.
///
/// Everything below the adapter is keyed by account id; `sender_id` is a
/// Telegram id. This is the single place the two meet, so a caller that
/// forgets to convert gets a type that is still an `i64` but a name that
/// says which one it is.
pub async fn account_of(core: &Core, telegram_id: i64) -> anyhow::Result<i64> {
    let store = core.deps.store.clone();
    crate::core::blocking(move || store.account_for_telegram(telegram_id)).await
}

/// Runs a `Store` call off the async executor. The connection is behind a
/// blocking mutex, so every one of these has to leave the reactor thread.
/// Which conversation this message belongs to.
///
/// After a long gap the old thread is set aside and a quick LLM check
/// decides whether the new message continues it — the same rule the
/// in-memory session used, now applied to stored conversations so it
/// survives a restart.
pub async fn resolve_conversation(
    core: &Core,
    account_id: i64,
    scope: &str,
    text: &str,
) -> anyhow::Result<i64> {
    let ttl = SESSION_TTL.as_secs() as i64;
    let store = core.deps.store.clone();
    let (scope_owned, latest) = {
        let scope_owned = scope.to_string();
        let s = scope_owned.clone();
        let store = store.clone();
        (
            scope_owned,
            crate::core::blocking(move || store.latest_conversation(account_id, &s, ttl)).await?,
        )
    };

    let Some((id, aged_out)) = latest else {
        let store = core.deps.store.clone();
        return crate::core::blocking(move || store.start_conversation(account_id, &scope_owned)).await;
    };
    if !aged_out {
        return Ok(id);
    }

    let excerpt = {
        let store = core.deps.store.clone();
        let history = crate::core::blocking(move || load_history(&store, id, HISTORY_CAP)).await?;
        last_messages_text(&history, 6)
    };
    if excerpt.trim().is_empty() {
        let store = core.deps.store.clone();
        return crate::core::blocking(move || store.start_conversation(account_id, &scope_owned)).await;
    }
    match crate::agent::continues_previous(&core.deps.llm, &excerpt, text).await {
        Ok(true) => {
            tracing::info!(account_id, id, "session expired but topic continues; keeping context");
            let store = core.deps.store.clone();
            crate::core::blocking(move || store.touch_conversation(id)).await?;
            Ok(id)
        }
        Ok(false) => {
            tracing::info!(account_id, "session expired; starting fresh");
            let store = core.deps.store.clone();
            crate::core::blocking(move || store.start_conversation(account_id, &scope_owned)).await
        }
        Err(e) => {
            tracing::warn!(error = %e, account_id, "continuation check failed; starting fresh");
            let store = core.deps.store.clone();
            crate::core::blocking(move || store.start_conversation(account_id, &scope_owned)).await
        }
    }
}

/// `Some(reply)` when this person has spent today's allowance.
///
/// Founders are exempt: they are the people paying for the bot.
///
/// A failed count lets the message through. The cap is a cost guard, not an
/// access control — the gate above it already decided this person is
/// allowed here — and a database blip should not silence everyone at once.
pub async fn over_daily_cap(core: &Core, user_id: i64) -> Option<String> {
    if core.is_founder(user_id) {
        return None;
    }
    let cap = core.cfg.invite_daily_requests;
    let store = core.deps.store.clone();
    let used = match crate::core::blocking(move || {
        let account_id = store.account_for_telegram(user_id)?;
        store.requests_today(account_id)
    })
    .await
    {
        Ok(used) => used,
        Err(e) => {
            tracing::warn!(error = %e, user_id, "daily cap check failed; letting it through");
            return None;
        }
    };
    (used >= cap).then(|| {
        tracing::info!(user_id, used, cap, "daily cap reached");
        format!("You've used today's {cap} requests. It resets at midnight UTC.")
    })
}

/// Rewrites a conversation's stored messages to match `history`.
///
/// A whole rewrite rather than an append: `trim_history` drops messages
/// from the front, so what is stored has to be what the agent will actually
/// be sent next time, not a growing log that disagrees with it.
pub(crate) fn save_history(store: &crate::store::Store, conversation_id: i64, history: &[LlmMessage]) -> anyhow::Result<()> {
    let bodies = history
        .iter()
        .map(serde_json::to_string)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    store.replace_messages(conversation_id, &bodies)
}

/// Stored messages, oldest first. A row that no longer deserializes —
/// because rig changed shape under us — is dropped rather than fatal:
/// losing some context is survivable, refusing to answer at all is not.
pub(crate) fn load_history(store: &crate::store::Store, conversation_id: i64, cap: usize) -> anyhow::Result<Vec<LlmMessage>> {
    let bodies = store.conversation_messages(conversation_id, cap)?;
    let mut out = Vec::with_capacity(bodies.len());
    for body in bodies {
        match serde_json::from_str::<LlmMessage>(&body) {
            Ok(m) => out.push(m),
            Err(e) => tracing::warn!(error = %e, "dropping an unreadable stored message"),
        }
    }
    Ok(out)
}

/// The last `n` plain-text messages of a history, rendered as
/// "user:/assistant:" lines for the continuation classifier.
pub(crate) fn last_messages_text(history: &[LlmMessage], n: usize) -> String {
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

/// Starts a fresh conversation, discarding whatever thread was live.
///
/// History lives in the store now, so clearing an in-memory slot would clear
/// nothing — /reset has to mean a new thread or it means nothing.
pub async fn reset(core: &Core, account_id: i64, scope: &str) -> anyhow::Result<i64> {
    let store = core.store();
    let scope = scope.to_string();
    crate::core::blocking(move || store.start_conversation(account_id, &scope)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_messages_text_renders_roles_and_takes_tail() {
                let history = vec![
            LlmMessage::user("oldest question"),
            LlmMessage::assistant("oldest answer"),
            LlmMessage::user("find me a bike"),
            LlmMessage::assistant("here are 3 bikes"),
        ];
        let excerpt = last_messages_text(&history, 2);
        assert_eq!(excerpt, "user: find me a bike\nassistant: here are 3 bikes");
    }

    #[test]
    fn history_survives_being_dropped_and_reloaded() {
        let (s, _d) = crate::store::tests::test_store();
        let a = s.account_for_telegram(11).unwrap();
        let c = s.start_conversation(a, "direct").unwrap();

        let original = vec![LlmMessage::user("cheapest beans"), LlmMessage::assistant("here")];
        save_history(&s, c, &original).unwrap();
        let loaded = load_history(&s, c, HISTORY_CAP).unwrap();

        // Not compared struct-for-struct: rig leaves `additional_params`
        // as None when a message is built in code but deserializes it to
        // Some({}), so a round trip is never byte-identical. What has to
        // hold is that the words and their roles survive, and that a second
        // trip changes nothing further — otherwise history would drift a
        // little every time it was reloaded.
        assert_eq!(loaded.len(), original.len());
        assert_eq!(last_messages_text(&loaded, 2), last_messages_text(&original, 2));

        save_history(&s, c, &loaded).unwrap();
        let again = load_history(&s, c, HISTORY_CAP).unwrap();
        assert_eq!(again, loaded, "reloading must reach a fixed point");
    }

    #[test]
    fn saving_replaces_rather_than_appends() {
        let (s, _d) = crate::store::tests::test_store();
        let a = s.account_for_telegram(11).unwrap();
        let c = s.start_conversation(a, "direct").unwrap();

        save_history(&s, c, &[LlmMessage::user("one"), LlmMessage::assistant("two")]).unwrap();
        // trim_history can drop from the front; the store must follow it
        // down rather than keeping the messages the agent will never see.
        save_history(&s, c, &[LlmMessage::assistant("two")]).unwrap();

        let loaded = load_history(&s, c, HISTORY_CAP).unwrap();
        assert_eq!(loaded.len(), 1, "a trimmed history must not grow back");
    }

    #[test]
    fn the_scope_of_a_private_chat_is_shared_and_a_group_is_not() {
        // Telegram makes chat id equal user id in a 1:1 chat, and that
        // thread is the one the web app will share. A group is a room with
        // other people in it and keeps its own history.
        assert_eq!(conversation_scope(4242, 4242), "direct");
        assert_eq!(conversation_scope(-100123, 4242), "telegram:-100123");
    }
}
