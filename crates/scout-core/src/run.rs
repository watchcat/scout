use crate::agent::{build_agent, wrap_up_agent, HISTORY_CAP, WRAP_UP_NOTE};
use crate::core::Core;
use crate::text::strip_thinking;
use futures::StreamExt;
use rig::agent::MultiTurnStreamItem;
use rig::completion::{Chat, Message as LlmMessage};
use rig::streaming::{StreamedAssistantContent, StreamingChat};

/// What a run produced.
///
/// `Busy` is not an error: asking two questions at once in one thread is an
/// ordinary thing to do. Each channel words it itself rather than core
/// writing chat copy.
pub enum RunOutcome {
    Answered(String),
    Busy,
    /// Every slot was taken by someone else's run for the whole of
    /// `QUEUE_WAIT`. Nothing was spent. Distinct from `Busy`, which is
    /// about this thread; this is about everybody else's.
    Overloaded,
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
/// The event a streamed chunk should produce, if anything changed.
///
/// A function rather than three lines inline, because `run_agent` needs a
/// live model and so nothing can test what happens inside it. Measured: with
/// this logic inline, reinstating the old `if !answer.is_empty()` guard —
/// which suppresses the retraction of reasoning already sent — was caught by
/// no test in the workspace. Here it is caught.
fn answer_event(
    shown: &mut scout_api::Shown,
    streamed: &str,
) -> Option<scout_api::AgentEvent> {
    shown.update(&strip_thinking(streamed)).map(scout_api::AgentEvent::Answer)
}

pub async fn run_agent(
    core: &Core,
    events: scout_api::EventSink,
    run: &scout_api::RunContext,
    prompt: &str,
) -> anyhow::Result<RunOutcome> {
    let Some(_guard) = begin_run(&core.deps.running, run.conversation_id) else {
        return Ok(RunOutcome::Busy);
    };
    let (account_id, conversation_id) = (run.account_id, run.conversation_id);
    // After the thread is claimed, so a second message in the same thread
    // is told "still working" rather than queued behind strangers; before
    // anything is built, because nothing below this line is free.
    let Some(_slot) = take_slot(&core.deps.runs, &events).await else {
        return Ok(RunOutcome::Overloaded);
    };
    let facts = {
        let store = core.deps.store.clone();
        tokio::task::spawn_blocking(move || store.list_facts(account_id)).await??
    };
    let agent = build_agent(&core.deps, run, &facts);
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
    // What each client has been shown, so the run can send the smallest
    // honest update rather than the whole text every token.
    let mut answer_shown = scout_api::Shown::default();
    let mut thinking_shown = scout_api::Shown::default();
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
                    // reasoning never reaches the chat as answer text — and
                    // when a stray closer proves the text so far *was*
                    // reasoning, the update is a Replace that takes it back.
                    // The old `if !answer.is_empty()` guard suppressed
                    // exactly that event, which is why it is gone.
                    if let Some(event) = answer_event(&mut answer_shown, &streamed) {
                        scout_api::emit(&events, event);
                    }
                }
                // MiniMax streams its reasoning on a separate channel. Shown
                // in italics while it works, replaced by the answer after.
                MultiTurnStreamItem::StreamAssistantItem(
                    StreamedAssistantContent::ReasoningDelta { reasoning, .. },
                ) => {
                    thinking.push_str(&reasoning);
                    if strip_thinking(&streamed).is_empty() {
                        if let Some(update) = thinking_shown.update(&thinking) {
                            scout_api::emit(&events, scout_api::AgentEvent::Thinking(update));
                        }
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
                        if let Some(update) = thinking_shown.update(&thinking) {
                            scout_api::emit(&events, scout_api::AgentEvent::Thinking(update));
                        }
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
        // Logged here rather than left to the channel: this is where the
        // conversation is known, and a channel that forgot would be silent
        // about the one thing worth knowing. The web channel did forget —
        // a run that died at turn 2 of 20 left the whole pod log without a
        // single WARN or ERROR in it, so there was nothing to diagnose.
        Ok(Err(e)) => {
            tracing::error!(error = %e, account_id, conversation_id, "the run failed");
            return Err(e);
        }
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
        tracing::warn!(conversation_id, reason, "run interrupted; writing up from notes");
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

    // The model sometimes writes a tool call out as prose instead of making
    // one. `rig` sees a response with no structured call in it, concludes
    // the agent has finished, and hands the markup back as the answer — so
    // nothing errors, nothing is logged, and the reader gets XML. Observed
    // on minimax-m3 at turn 4 of 20.
    //
    // One corrective turn, because the research behind it is real and worth
    // keeping. Ordered before the dead-link check on purpose: there is no
    // point resolving urls inside markup, and the repaired answer is what
    // the reader will actually see, so it is the one that has to be
    // checked.
    if crate::toolcall::looks_like_tool_call(&reply) {
        tracing::warn!(conversation_id, "the model wrote a tool call as text; asking it to answer");
        reply = strip_thinking(&agent.chat(crate::toolcall::REPAIR_NOTE, &mut history).await?);
        if crate::toolcall::looks_like_tool_call(&reply) {
            // Twice is not a slip. The run has not answered, and an apology
            // the reader understands beats markup they cannot.
            anyhow::bail!("the model wrote a tool call as text instead of answering, twice");
        }
    }

    // Guard against links the model wrote but never saw: an invented Amazon
    // /dp/<ASIN> URL reads as a real product page and answers 404. One repair
    // turn, then scrub whatever is still dead.
    let dead = crate::links::dead_links_in(&core.deps.http, &reply).await;
    if !dead.is_empty() {
        tracing::warn!(?dead, conversation_id, "dead links in reply; asking the agent to correct it");
        let note = crate::links::repair_prompt(&dead);
        reply = strip_thinking(&agent.chat(note, &mut history).await?);
        let still_dead = crate::links::dead_links_in(&core.deps.http, &reply).await;
        if !still_dead.is_empty() {
            tracing::warn!(?still_dead, conversation_id, "dead links survived the correction; stripping");
            reply = crate::links::strike_dead(&reply, &still_dead);
        }
    }

    trim_history(&mut history, HISTORY_CAP);
    let store = core.deps.store.clone();
    match crate::core::blocking(move || crate::session::save_history(&store, conversation_id, &history)).await {
        // The answer is already on its way to the user; losing the thread is
        // worse than not saving it, but it is not worth failing the reply.
        Err(e) => tracing::warn!(error = %e, conversation_id, "could not save the conversation"),
        // Named only once saved, so a thread that failed to save is not
        // named as if it had. Writes only over a null title, so a rename
        // survives. Named from the person's words, never the prompt.
        Ok(()) => {
            if let Some(source) = &run.title_source {
                crate::session::title_if_missing(core, conversation_id, source).await;
            }
        }
    }
    Ok(RunOutcome::Answered(reply))
}

/// Runs allowed at once across every conversation and channel.
///
/// The per-conversation claim above stops one person from stacking runs;
/// this stops a hundred people from doing it at the same moment. Each run
/// is up to `MAX_TURNS` model calls plus searches billed per query, on one
/// key for the whole process, so a burst is a bill as much as a load.
pub const MAX_CONCURRENT_RUNS: usize = 8;

/// How long a run will wait for a slot before giving up.
///
/// Long enough that a burst clears — a slot frees every time a run ends,
/// and most end well inside a minute — and short enough that nobody waits
/// longer for their turn than the answer itself would take.
const QUEUE_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Takes one of the process-wide run slots, saying so if it has to wait.
///
/// Silent when a slot is free: the common case should read exactly as it
/// did before the cap existed. `None` after `QUEUE_WAIT`, having spent
/// nothing — the caller turns that into an outcome the channel can word.
pub(crate) async fn take_slot(
    runs: &std::sync::Arc<tokio::sync::Semaphore>,
    events: &scout_api::EventSink,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    if let Ok(slot) = runs.clone().try_acquire_owned() {
        return Some(slot);
    }
    scout_api::emit(
        &events.clone(),
        scout_api::AgentEvent::Notice(
            "⏳ Scout is busy with other requests; yours is queued".to_string(),
        ),
    );
    tokio::time::timeout(QUEUE_WAIT, runs.clone().acquire_owned())
        .await
        .ok()
        .and_then(|acquired| acquired.ok())
}

/// Held for the length of a run. Dropping it frees the conversation, so a
/// panic, a timeout or a dropped future cannot wedge a thread forever —
/// which an insert/remove pair around the body would.
pub(crate) struct RunGuard {
    running: std::sync::Arc<dashmap::DashSet<i64>>,
    conversation_id: i64,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.running.remove(&self.conversation_id);
    }
}

/// Claims a conversation, or `None` if a run already holds it.
///
/// `DashSet::insert` reports whether the value was new, which makes this an
/// atomic check-and-claim rather than a check followed by a claim.
pub(crate) fn begin_run(
    running: &std::sync::Arc<dashmap::DashSet<i64>>,
    conversation_id: i64,
) -> Option<RunGuard> {
    running
        .insert(conversation_id)
        .then(|| RunGuard { running: running.clone(), conversation_id })
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

    #[test]
    fn a_tool_call_written_as_text_is_repaired_before_the_links_are_checked() {
        // Neither branch is reachable without a live model, so the ordering
        // is asserted from the source instead. Resolving urls that sit
        // inside markup is wasted work, and the repaired answer is the one
        // the reader actually sees — so it is the one that has to be
        // link-checked.
        let src = include_str!("run.rs");
        let repair = src.find("looks_like_tool_call").expect("the repair must exist");
        let links = src.find("dead_links_in").expect("the link check must exist");
        assert!(repair < links, "the tool-call repair must run before the dead-link check");
    }

    fn slots(n: usize) -> std::sync::Arc<tokio::sync::Semaphore> {
        std::sync::Arc::new(tokio::sync::Semaphore::new(n))
    }

    #[tokio::test(start_paused = true)]
    async fn a_free_slot_is_taken_without_a_word() {
        let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();

        let slot = take_slot(&slots(1), &events).await;

        assert!(slot.is_some());
        assert!(seen.try_recv().is_err(), "nothing waited, so nothing should have been said");
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_house_says_so_and_then_proceeds_when_a_slot_frees() {
        let runs = slots(1);
        let held = runs.clone().acquire_owned().await.unwrap();
        let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();

        let waiting = tokio::spawn({
            let runs = runs.clone();
            async move { take_slot(&runs, &events).await }
        });
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        assert!(
            matches!(seen.try_recv(), Ok(scout_api::AgentEvent::Notice(ref n)) if n.contains("queued")),
            "the person waiting was not told they were queued"
        );

        drop(held);
        let slot = tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .expect("a freed slot should end the wait")
            .unwrap();
        assert!(slot.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_past_the_limit_gives_up() {
        let runs = slots(1);
        let _held = runs.clone().acquire_owned().await.unwrap();
        let (events, _seen) = tokio::sync::mpsc::unbounded_channel();

        let slot = take_slot(&runs, &events).await;

        assert!(slot.is_none(), "a run that never got a slot must not pretend it did");
    }

    #[test]
    fn every_run_takes_a_slot_before_it_spends_anything() {
        // The slot has to be taken after the conversation is claimed — a
        // second message in the same thread must be told "still working",
        // not queued behind strangers — and before the agent is built,
        // because building it is where the model calls start.
        let src = include_str!("run.rs");
        let src = &src[..src.find("#[cfg(test)]").expect("the tests must come last")];
        let claim = src.find("begin_run(").expect("the conversation claim must exist");
        let slot = src.find("take_slot(").expect("the run cap must exist");
        let build = src.find("build_agent(").expect("the agent build must exist");
        assert!(claim < slot && slot < build, "the run cap is in the wrong place");
    }

    #[test]
    fn every_answered_run_names_a_thread_that_has_no_name_yet() {
        // Telegram never shows titles, so a thread started there would sit
        // nameless in the sidebar forever if only the web path titled it.
        // The one place both channels pass through is here.
        let src = include_str!("run.rs");
        let src = &src[..src.find("#[cfg(test)]").expect("the tests must come last")];
        let src = &src[src.find("pub async fn run_agent").expect("the run must exist")..];
        let saved = src.find("save_history(").expect("the save must exist");
        let titled = src.find("title_if_missing(").expect("the title must be set");
        assert_eq!(src.matches("title_if_missing(").count(), 1, "the thread is named more than once");
        assert!(saved < titled, "the title is set before the history is saved");
        // Named from the person's words. The prompt is not the message:
        // Telegram appends a `[system note]` to a price request, and a
        // title cut from that reads `cheapest usb hub [system note] This…`.
        assert!(src.contains("run.title_source"), "the thread is named from something other than the caller's title source");
    }

    #[test]
    fn a_stray_closer_makes_the_run_retract_the_answer_it_already_sent() {
        // `Shown` is tested on its own, but this is the wiring: a guard
        // added here would swallow the retraction while every unit test
        // stayed green. That is not hypothetical — it was true until this
        // test existed.
        let source = "secret reasoning here</think>The answer";
        let mut shown = scout_api::Shown::default();

        assert!(
            matches!(
                answer_event(&mut shown, &source[..21]),
                Some(scout_api::AgentEvent::Answer(scout_api::TextUpdate::Append(ref t)))
                    if t == "secret reasoning here"
            ),
            "reasoning should reach the client before the closer proves what it is"
        );
        assert!(
            matches!(
                answer_event(&mut shown, &source[..29]),
                Some(scout_api::AgentEvent::Answer(scout_api::TextUpdate::Replace(ref t)))
                    if t.is_empty()
            ),
            "the run did not retract reasoning it had already sent"
        );
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
    fn a_conversation_admits_one_run_and_frees_itself_when_it_ends() {
        // Two runs on one thread both load the history and both write it
        // back wholesale, so the second erases the first's exchange. A
        // laptop and a phone on the shared `direct` thread make that
        // ordinary rather than rare.
        let running = std::sync::Arc::new(dashmap::DashSet::new());
        let first = begin_run(&running, 7).expect("the first run should start");
        assert!(begin_run(&running, 7).is_none(), "a second run got in");
        // A different thread is unaffected.
        assert!(begin_run(&running, 8).is_some());

        drop(first);
        assert!(
            begin_run(&running, 7).is_some(),
            "the conversation stayed locked after its run ended"
        );
    }
}
