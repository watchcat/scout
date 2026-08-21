use crate::agent::{build_agent, wrap_up_agent, HISTORY_CAP, WRAP_UP_NOTE};
use crate::core::Core;
use crate::text::strip_thinking;
use futures::StreamExt;
use rig::agent::MultiTurnStreamItem;
use rig::completion::{Chat, Message as LlmMessage};
use rig::streaming::{StreamedAssistantContent, StreamingChat};

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
pub async fn run_agent(
    core: &Core,
    events: scout_api::EventSink,
    user_id: i64,
    chat_id: i64,
    conversation_id: i64,
    prompt: &str,
) -> anyhow::Result<String> {
    let account_id = crate::session::account_of(core, user_id).await?;
    let facts = {
        let store = core.deps.store.clone();
        tokio::task::spawn_blocking(move || store.list_facts(account_id)).await??
    };
    let agent = build_agent(&core.deps, account_id, chat_id, &facts);
    // History comes from the conversation the caller opened, so an
    // in-flight run always reads and writes that thread and never anyone
    // else's — the isolation the (chat, user) map used to provide.
    let mut history = {
        let store = core.deps.store.clone();
        crate::core::blocking(move || crate::session::load_history(&store, conversation_id, HISTORY_CAP)).await?
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
                    scout_api::emit(
                        &events,
                        scout_api::AgentEvent::Tool(crate::describe::describe(
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
                        scout_api::emit(&events, scout_api::AgentEvent::Answer(answer));
                    }
                }
                // MiniMax streams its reasoning on a separate channel. Shown
                // in italics while it works, replaced by the answer after.
                MultiTurnStreamItem::StreamAssistantItem(
                    StreamedAssistantContent::ReasoningDelta { reasoning, .. },
                ) => {
                    thinking.push_str(&reasoning);
                    if strip_thinking(&streamed).is_empty() {
                        scout_api::emit(
                            &events,
                            scout_api::AgentEvent::Thinking(thinking.clone()),
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
                        scout_api::emit(
                            &events,
                            scout_api::AgentEvent::Thinking(thinking.clone()),
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
        scout_api::emit(
            &events,
            scout_api::AgentEvent::Notice(
                "✍️ wrapping up with what I found so far".to_string(),
            ),
        );
        // The history of an interrupted run is never returned, so the
        // model's own notes are the material: its reasoning already lists
        // the prices it confirmed.
        let notes = tail_chars(&format!("{thinking}\n\n{streamed}"), WRAP_UP_CONTEXT);
        let wrap_up = wrap_up_agent(&core.deps, &facts);
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
    let dead = crate::links::dead_links_in(&core.deps.http, &reply).await;
    if !dead.is_empty() {
        tracing::warn!(?dead, chat_id, "dead links in reply; asking the agent to correct it");
        let note = crate::links::repair_prompt(&dead);
        reply = strip_thinking(&agent.chat(note, &mut history).await?);
        let still_dead = crate::links::dead_links_in(&core.deps.http, &reply).await;
        if !still_dead.is_empty() {
            tracing::warn!(?still_dead, chat_id, "dead links survived the correction; stripping");
            reply = crate::links::strike_dead(&reply, &still_dead);
        }
    }

    trim_history(&mut history, HISTORY_CAP);
    let store = core.deps.store.clone();
    if let Err(e) = crate::core::blocking(move || crate::session::save_history(&store, conversation_id, &history)).await {
        // The answer is already on its way to the user; losing the thread is
        // worse than not saving it, but it is not worth failing the reply.
        tracing::warn!(error = %e, conversation_id, "could not save the conversation");
    }
    Ok(reply)
}

/// Turn an agent failure into a user-facing message; the max-turns budget
/// gets an actionable explanation instead of the generic apology.
pub fn agent_error_message(e: &anyhow::Error) -> &'static str {
    if e.to_string().contains("max turns") {
        "That request needed more research steps than I allow per message. \
         Try narrowing it (a more specific product, or fewer platforms), or \
         ask me to continue from where I stopped."
    } else {
        "Sorry, something went wrong on my side. Please try again."
    }
}

/// The last `max` characters — the newest notes are the ones carrying
/// confirmed prices.
fn tail_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(max)).collect()
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

/// rig wraps the turn-limit failure in its own error types; the message is
/// the stable part across them.
fn is_max_turns(e: &impl std::fmt::Display) -> bool {
    let text = e.to_string();
    text.contains("MaxTurnsError") || text.contains("max turns")
}

#[cfg(test)]
mod tests {
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
}
