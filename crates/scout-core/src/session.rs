use crate::agent::HISTORY_CAP;
use crate::core::Core;
use rig::completion::Message as LlmMessage;

/// A conversation is set aside after this long without a word, and a quick
/// LLM check decides whether the next message resumes it.
pub const SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// The account behind a Telegram user, created on first sight.
///
/// The one place the two id spaces meet. Everything below is keyed by
/// account id, and the argument type is now what stops a caller passing the
/// wrong number: it used to be an `i64` with a hopeful name.
pub async fn account_of(core: &Core, id: crate::ids::TelegramId) -> anyhow::Result<i64> {
    let store = core.deps.store.clone();
    crate::core::blocking(move || store.account_for_telegram(id.0)).await
}

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
pub async fn over_daily_cap(core: &Core, account_id: i64) -> Option<String> {
    match core.founder_account(account_id).await {
        Ok(true) => return None,
        Ok(false) => {}
        // Same reasoning as a failed count below: the cap is a cost guard,
        // not access control, and a database blip must not silence everyone.
        Err(e) => {
            tracing::warn!(error = %e, account_id, "founder check failed; letting it through");
            return None;
        }
    }
    let cap = core.cfg.invite_daily_requests;
    let store = core.deps.store.clone();
    let used = match crate::core::blocking(move || store.requests_today(account_id)).await {
        Ok(used) => used,
        Err(e) => {
            tracing::warn!(error = %e, account_id, "daily cap check failed; letting it through");
            return None;
        }
    };
    (used >= cap).then(|| {
        tracing::info!(account_id, used, cap, "daily cap reached");
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

/// The sayable text of a user turn, or `None` when it holds only tool
/// results — the rule shared by `last_messages_text` and `transcript_of` so
/// what counts as "something a person said" cannot drift between the two.
fn text_of_user(content: &rig::OneOrMany<rig::message::UserContent>) -> Option<String> {
    content
        .iter()
        .filter_map(|c| match c {
            rig::message::UserContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .reduce(|a, b| format!("{a}\n{b}"))
}

/// The sayable text of an assistant turn, or `None` when it holds only tool
/// calls — see `text_of_user`.
fn text_of_assistant(content: &rig::OneOrMany<rig::message::AssistantContent>) -> Option<String> {
    content
        .iter()
        .filter_map(|c| match c {
            rig::message::AssistantContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .reduce(|a, b| format!("{a}\n{b}"))
}

/// The last `n` plain-text messages of a history, rendered as
/// "user:/assistant:" lines for the continuation classifier.
pub(crate) fn last_messages_text(history: &[LlmMessage], n: usize) -> String {
    let mut lines: Vec<String> = history
        .iter()
        .rev()
        .filter_map(|m| match m {
            LlmMessage::User { content } => text_of_user(content).map(|t| format!("user: {t}")),
            LlmMessage::Assistant { content, .. } => text_of_assistant(content).map(|t| format!("assistant: {t}")),
            _ => None,
        })
        .take(n)
        .collect();
    lines.reverse();
    lines.join("\n")
}

/// The conversation a page should show, without starting one.
///
/// `resolve_conversation` is wrong here twice over: it needs the message
/// text, because after a long gap it asks the model whether the new message
/// continues the old thread, and it creates a conversation when there is
/// none. Opening a page must create nothing.
pub(crate) fn latest_direct(store: &crate::store::Store, account_id: i64) -> anyhow::Result<Option<i64>> {
    let ttl = SESSION_TTL.as_secs() as i64;
    Ok(store.latest_conversation(account_id, "direct", ttl)?.map(|(id, _aged)| id))
}

/// What was said, oldest first, with the tool traffic left out.
///
/// The same line `last_messages_text` draws, kept structured rather than
/// flattened: that one exists to feed a classifier a blob, this one exists
/// to be rendered.
pub(crate) fn transcript_of(store: &crate::store::Store, conversation_id: i64) -> anyhow::Result<Vec<scout_api::Turn>> {
    Ok(turns_of(&load_history(store, conversation_id, HISTORY_CAP)?))
}

/// Whether an assistant message is an answer rather than a step of one.
///
/// A message that carries a tool call is the model working. Whatever text
/// sits beside the call is what it says on its way to an answer — "let me
/// check the next shop" — and the finished reply is the turn with no call
/// left to make, because had there been one the agent would have kept
/// going. So the presence of a call, not the absence of text, is the test.
fn is_answer(content: &rig::OneOrMany<rig::message::AssistantContent>) -> bool {
    !content
        .iter()
        .any(|c| matches!(c, rig::message::AssistantContent::ToolCall(_)))
}

/// Whether the answer at `i` was replaced by the one after it.
///
/// The repair paths hand the model a correction and keep its second attempt,
/// but `rig` appends rather than replaces — so the history holds both. Left
/// in, a transcript shows the superseded answer *and* its replacement: the
/// dead links the repair existed to remove, or the raw `<tool_call>` markup
/// the model wrote as prose, sitting above the real answer forever.
///
/// A thumbs-up follow-up also opens with a system note and must NOT trigger
/// this — the answer above that one still stands. `CORRECTION_NOTE` is what
/// separates the two.
fn superseded(history: &[LlmMessage], i: usize) -> bool {
    let Some(LlmMessage::User { content }) = history.get(i + 1) else {
        return false;
    };
    text_of_user(content).is_some_and(|t| t.starts_with(crate::text::CORRECTION_NOTE))
}

/// The turns a history should be rendered as, oldest first.
///
/// Split out of `transcript_of` because the store half needs a database and
/// this half is the part that can be wrong: which messages count as things
/// that were said, and what is stripped from them before they are shown.
fn turns_of(history: &[LlmMessage]) -> Vec<scout_api::Turn> {
    history
        .iter()
        .enumerate()
        .filter_map(|(i, m)| match m {
            // Cut, because `rig`'s `Chat::chat` appends the prompt it is
            // given to the history — so the dead-link repair's own
            // instruction was saved as a user message and rendered back to
            // the person as something they had said.
            LlmMessage::User { content } => text_of_user(content).map(|text| scout_api::Turn {
                role: scout_api::Role::You,
                text: crate::text::said_by_person(&text).to_string(),
            }),
            // Stripped, because what is *stored* is the model's raw message:
            // `run_agent` saves `res.messages()`, tags and all, and only the
            // reply it hands the channel goes through `strip_thinking`.
            // Telegram never renders history so this never showed; the web
            // page does, and rendering it raw put a whole chain of thought
            // on screen — permanently, on every load, rather than for the
            // moment a stream does.
            LlmMessage::Assistant { content, .. }
                if is_answer(content) && !superseded(history, i) =>
            {
                text_of_assistant(content).map(|text| scout_api::Turn {
                    role: scout_api::Role::Scout,
                    text: crate::text::strip_thinking(&text),
                })
            }
            _ => None,
        })
        .filter(|t| !t.text.trim().is_empty())
        .collect()
}

/// The current thread's transcript, or empty when there is no thread yet.
pub async fn transcript(core: &Core, account_id: i64) -> anyhow::Result<Vec<scout_api::Turn>> {
    let store = core.store();
    crate::core::blocking(move || {
        let Some(id) = latest_direct(&store, account_id)? else {
            return Ok(Vec::new());
        };
        transcript_of(&store, id)
    })
    .await
}

/// The current thread's id and transcript, or `None` when there is no
/// thread yet.
///
/// `transcript` answers the page, which only needs the turns. Mirroring
/// needs the id as well, because a turn's key is scoped to its conversation
/// — the same question asked again in a new thread is a new turn.
pub async fn current_thread(
    core: &Core,
    account_id: i64,
) -> anyhow::Result<Option<(i64, Vec<scout_api::Turn>)>> {
    let store = core.store();
    crate::core::blocking(move || {
        let Some(id) = latest_direct(&store, account_id)? else {
            return Ok(None);
        };
        Ok(Some((id, transcript_of(&store, id)?)))
    })
    .await
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

/// Records an exchange as though it had already happened, without spending
/// a model call.
///
/// `Store` and `save_history` stay private to this crate — that boundary is
/// deliberate, see the module doc at the crate root — so this is the one
/// door a caller outside the crate has to them. It exists for tests that
/// need `transcript` to return something real: the only other public writer
/// of history is `run::run_agent`, which means talking to a live model.
///
/// Named for what it is so that nobody reaches for it in earnest: a channel
/// writing an exchange nobody had is a channel inventing history.
#[doc(hidden)]
pub async fn seed_exchange_for_tests(
    core: &Core,
    account_id: i64,
    scope: &str,
    you_said: &str,
    scout_said: &str,
) -> anyhow::Result<i64> {
    let store = core.store();
    let scope = scope.to_string();
    let (you_said, scout_said) = (you_said.to_string(), scout_said.to_string());
    crate::core::blocking(move || {
        let id = store.start_conversation(account_id, &scope)?;
        save_history(&store, id, &[LlmMessage::user(you_said), LlmMessage::assistant(scout_said)])?;
        Ok(id)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn narrating_tool_call(text: &str, tool: &str) -> LlmMessage {
        // What a multi-turn run actually stores for a step: the model says
        // what it is about to do, and calls the tool in the same message.
        LlmMessage::Assistant {
            id: None,
            content: rig::OneOrMany::many([
                rig::message::AssistantContent::text(text),
                rig::message::AssistantContent::ToolCall(rig::message::ToolCall::new(
                    "call-1".to_string(),
                    rig::message::ToolFunction {
                        name: tool.to_string(),
                        arguments: serde_json::json!({}),
                    },
                )),
            ])
            .unwrap(),
        }
    }

    #[test]
    fn an_instruction_scout_gave_itself_is_not_shown_as_the_persons_words() {
        // `rig`'s `Chat::chat` appends the prompt to the history, so the
        // dead-link repair — which runs on exactly the price answers this
        // bot exists to write — left its own instruction in the transcript
        // wearing the reader's name.
        let history = vec![
            LlmMessage::user("find me cheapest gillette\n\n[system note] This is a cheapest-price request."),
            LlmMessage::user(crate::links::repair_prompt(&["https://bol.com/404".to_string()])),
            LlmMessage::assistant("EUR 40.44 delivered"),
        ];
        let turns = turns_of(&history);
        assert_eq!(turns.len(), 2, "got: {:?}", turns.iter().map(|t| &t.text).collect::<Vec<_>>());
        assert_eq!(turns[0].text, "find me cheapest gillette");
        assert_eq!(turns[1].text, "EUR 40.44 delivered");
    }

    #[test]
    fn the_narration_a_run_writes_between_tool_calls_is_not_a_turn() {
        // Measured on a real transcript: a price comparison rendered as a
        // page of "Let me check Kruidvat, ShaveSavings and bol.com", then
        // "The shavesavings 8-pack is out of stock", then the answer — one
        // bubble per step of the run, permanently, on every reload.
        //
        // A message carrying a tool call is the model working. The text
        // beside the call is what it says on its way to an answer, not the
        // answer: the finished reply is the turn with no call left to make,
        // because had there been one the agent would have kept going.
        let history = vec![
            LlmMessage::user("find me cheapest gillette cartridges"),
            narrating_tool_call("Let me check Kruidvat, ShaveSavings and bol.com.", "search"),
            narrating_tool_call("The 8-pack is out of stock. Let me check delivery.", "fetch_page"),
            LlmMessage::assistant("EUR 40.44 delivered — 16-pack, shavesavings.com"),
        ];
        let turns = turns_of(&history);
        assert_eq!(turns.len(), 2, "got: {:?}", turns.iter().map(|t| &t.text).collect::<Vec<_>>());
        assert_eq!(turns[0].text, "find me cheapest gillette cartridges");
        assert_eq!(turns[1].text, "EUR 40.44 delivered — 16-pack, shavesavings.com");
    }

    #[test]
    fn an_answer_that_was_corrected_is_not_shown_beside_its_replacement() {
        // `rig` appends the repair exchange rather than replacing it, so the
        // history holds the answer with the dead links *and* the corrected
        // one. Rendering both leaves the links this bot spent a request
        // removing sitting above the answer, permanently — and mirroring
        // both sends them to the reader's phone.
        let history = vec![
            LlmMessage::user("cheapest beans"),
            LlmMessage::assistant("EUR 3 at https://dead.example/404"),
            LlmMessage::user(crate::links::repair_prompt(&["https://dead.example/404".to_string()])),
            LlmMessage::assistant("EUR 3 at bol.com"),
        ];
        let turns = turns_of(&history);
        assert_eq!(turns.len(), 2, "got: {:?}", turns.iter().map(|t| &t.text).collect::<Vec<_>>());
        assert_eq!(turns[1].text, "EUR 3 at bol.com");
    }

    #[test]
    fn a_thumbs_up_does_not_supersede_the_answer_it_praised() {
        // The reaction follow-up also opens with a system note, but the
        // answer above it stands — dropping it would delete a real reply
        // from the reader's transcript for having been liked.
        let history = vec![
            LlmMessage::assistant("EUR 3 at bol.com"),
            LlmMessage::user("[system note] The user reacted with a thumbs-up to this earlier reply."),
            LlmMessage::assistant("Want me to save that one?"),
        ];
        let turns = turns_of(&history);
        assert_eq!(turns.len(), 2, "got: {:?}", turns.iter().map(|t| &t.text).collect::<Vec<_>>());
        assert_eq!(turns[0].text, "EUR 3 at bol.com");
    }

    #[test]
    fn a_rendered_turn_still_has_its_reasoning_stripped() {
        // The other half of the same guarantee: what is stored is the
        // model's raw message, tags and all, so the answer turn is stripped
        // at render time — including the namespaced spelling.
        let history = vec![LlmMessage::assistant("my reasoning</mm:think>The answer")];
        assert_eq!(turns_of(&history)[0].text, "The answer");
    }

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
    fn a_transcript_is_the_exchange_without_the_tool_traffic() {
        // History holds tool calls and their results as well as the
        // conversation. A page shows what was said, not how it was found —
        // and `last_messages_text` already draws that line for the
        // continuation classifier, so this draws the same one.
        use scout_api::{Role, Turn};
        let (s, _d) = crate::store::tests::test_store();
        let a = s.account_for_telegram(11).unwrap();
        let c = s.start_conversation(a, "direct").unwrap();

        // A real exchange has the tool call and its result sitting between
        // the question and the answer. Without them in this fixture the
        // test's own name would be a claim it never checks.
        use rig::completion::message::{AssistantContent, ToolResult, ToolResultContent, UserContent};
        use rig::message::{ToolCall, ToolFunction};
        use rig::one_or_many::OneOrMany;
        save_history(
            &s,
            c,
            &[
                LlmMessage::user("cheapest beans"),
                LlmMessage::Assistant {
                    id: None,
                    content: OneOrMany::one(AssistantContent::ToolCall(ToolCall::new(
                        "call-1".to_string(),
                        ToolFunction {
                            name: "web_search".to_string(),
                            arguments: serde_json::json!({}),
                        },
                    ))),
                },
                LlmMessage::User {
                    content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                        id: "call-1".to_string(),
                        call_id: None,
                        content: OneOrMany::one(ToolResultContent::text("three shops")),
                    })),
                },
                LlmMessage::assistant("here are three"),
            ],
        )
        .unwrap();

        assert_eq!(
            transcript_of(&s, c).unwrap(),
            vec![
                Turn { role: Role::You, text: "cheapest beans".into() },
                Turn { role: Role::Scout, text: "here are three".into() },
            ]
        );
    }

    #[test]
    fn a_transcript_does_not_show_the_reasoning_stored_beside_the_answer() {
        // What is stored is the model's raw message. The live stream strips
        // it; a page reading history has to strip it too, or a chain of
        // thought sits on screen every time the page is opened.
        let (s, _d) = crate::store::tests::test_store();
        let a = s.account_for_telegram(11).unwrap();
        let c = s.start_conversation(a, "direct").unwrap();
        save_history(
            &s,
            c,
            &[
                LlmMessage::user("test"),
                LlmMessage::assistant(
                    "<think>\nThe user just sent \"test\". I should respond briefly.\n</think>\n\nHi! Scout here.",
                ),
            ],
        )
        .unwrap();

        let turns = transcript_of(&s, c).unwrap();
        assert_eq!(turns[1].text, "Hi! Scout here.");
        assert!(
            !turns.iter().any(|t| t.text.contains("<think>")),
            "reasoning reached the page: {turns:?}"
        );
    }

    #[test]
    fn a_conversation_that_was_never_started_is_not_started_by_asking() {
        // Opening a page must not write rows. `resolve_conversation` would
        // create one, which is why the page does not use it.
        let (s, _d) = crate::store::tests::test_store();
        let a = s.account_for_telegram(11).unwrap();
        assert_eq!(latest_direct(&s, a).unwrap(), None);
        assert_eq!(latest_direct(&s, a).unwrap(), None, "asking twice created one");
    }
}
