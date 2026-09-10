//! A specialist agent the main agent calls as a tool.
//!
//! The wrapper knows nothing about flights. It streams a nested run, tells
//! the chat what the nested tools are doing, keeps every nested tool's real
//! output, and hands the parent a report: the specialist's own sentence,
//! the findings, and whatever presentation rules the instance says apply
//! to those findings. Numbers reach the parent through the findings, never
//! through the nested model's prose.

use std::collections::HashMap;

/// One nested tool call and what it returned, as the tool returned it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Finding {
    pub tool: String,
    pub args: serde_json::Value,
    /// The tool's own JSON, or its text when it did not return JSON.
    pub output: serde_json::Value,
    /// True when the tool returned an error, which rig hands to the model
    /// as text; `output` is then that text. Every Scout tool returns a
    /// serialised struct, so "not JSON" is the signal.
    pub failed: bool,
}

/// What the parent agent receives.
#[derive(Debug, serde::Serialize)]
pub struct Report {
    /// The specialist's own one or two sentences: what it searched and any
    /// caveat. When the run was cut short, that is said here too.
    pub summary: String,
    pub findings: Vec<Finding>,
    /// Presentation rules that apply to these findings, chosen in Rust.
    pub guidance: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SpecialistError {
    /// Phrased for the model to relay, not as a diagnostic.
    #[error("the specialist could not answer: {0}")]
    Failed(String),
}

/// What the chat is told once no nested call is outstanding any more.
///
/// True whichever way the run goes from there — another tool call, or the
/// answer being written — because whatever comes next replaces this line,
/// and a line promising the answer is nearly here would be a lie half the
/// time.
const WORKING: &str = "🧠 working through what came back";

/// Gathers a nested run as it streams.
#[derive(Debug, Default)]
pub(crate) struct Collector {
    /// Calls started and not yet answered, by rig's internal call id.
    pending: HashMap<String, (String, serde_json::Value)>,
    pub(crate) findings: Vec<Finding>,
    summary: String,
}

impl Collector {
    /// A nested tool began: tell the chat, remember the call.
    pub(crate) fn tool_started(
        &mut self,
        call_id: &str,
        tool: &str,
        args: serde_json::Value,
        events: &scout_api::EventSink,
    ) {
        scout_api::emit(events, scout_api::AgentEvent::Tool(crate::describe::describe(tool, &args)));
        self.pending.insert(call_id.to_string(), (tool.to_string(), args));
    }

    /// A nested tool answered. Its text is parsed back into the JSON the
    /// tool produced, so the parent reads the same shape it would have
    /// read from calling the tool itself.
    ///
    /// Answering the last outstanding call is also the moment this file
    /// goes quiet: nothing else here speaks until the next tool starts, and
    /// once the flight-search cap was reached there was no next tool. The
    /// chat sat on "✈️ searching HKG→FUK" for the two minutes the model
    /// spent writing its answer (production, 08:19:45 to 08:21:45), so the
    /// reader was told the search was still running long after it had
    /// stopped.
    pub(crate) fn tool_finished(
        &mut self,
        call_id: &str,
        text: &str,
        events: &scout_api::EventSink,
    ) {
        let Some((tool, args)) = self.pending.remove(call_id) else {
            return;
        };
        let (output, failed) = match serde_json::from_str(text) {
            Ok(v) => (v, false),
            Err(_) => (serde_json::Value::String(text.to_string()), true),
        };
        self.findings.push(Finding { tool, args, output, failed });
        // Only when nothing is left running: a call still in flight has its
        // own line, which names what it is doing and so says more.
        if self.pending.is_empty() {
            scout_api::emit(events, scout_api::AgentEvent::Tool(WORKING.to_string()));
        }
    }

    /// The nested model's final text.
    pub(crate) fn finished(&mut self, output: &str) {
        self.summary = crate::text::strip_thinking(output);
    }

    /// The report, or an error when the run was cut short with nothing to
    /// show. `cut_short` is the reason when the run did not finish on its
    /// own; it is written into the summary so the parent knows.
    ///
    /// Failed findings are handed over too, so the parent can say what
    /// went wrong; guidance functions should count only findings with
    /// `failed == false`.
    pub(crate) fn report(
        self,
        cut_short: Option<&str>,
        guidance: &dyn Fn(&[Finding]) -> Vec<String>,
    ) -> Result<Report, SpecialistError> {
        // A summary that is itself a tool call is not an answer. The nested
        // model sometimes writes the call out as prose instead of making
        // one (minimax-m3, measured in production; see the repair turn in
        // run.rs). Here nothing repairs it: rig sees no structured call,
        // ends the stream as a FinalResponse, and the markup lands in the
        // summary with no finding behind it, which would otherwise pass
        // as a clean, empty report. Only when nothing succeeded: a real
        // finding is still worth handing over, whatever the prose says.
        let nothing_succeeded = self.findings.iter().all(|f| f.failed);
        let cut_short = match cut_short {
            None if nothing_succeeded && crate::toolcall::looks_like_tool_call(&self.summary) => {
                Some("it wrote a tool call as text instead of making one")
            }
            other => other,
        };
        let summary = match cut_short {
            None => self.summary,
            Some(reason) if self.findings.iter().all(|f| f.failed) => {
                return Err(SpecialistError::Failed(reason.to_string()))
            }
            Some(reason) => format!(
                "The lookup was cut short ({reason}); these findings are what was gathered before that."
            ),
        };
        let guidance = guidance(&self.findings);
        Ok(Report { summary, findings: self.findings, guidance })
    }
}

/// How long one nested run may take. Inside the outer run's own budget,
/// and long enough for a flexible-window search that fans out over a week.
pub const SPECIALIST_BUDGET: std::time::Duration = std::time::Duration::from_secs(180);

/// A nested stream silent for this long is stalled rather than thinking.
///
/// Shorter than the outer run's ninety seconds on purpose. That guard
/// watches a stream which is silent for the whole of a specialist run by
/// design, so it has to be generous; this one watches the nested stream
/// itself, where every token of the model's own writing arrives as an item
/// and the longest honest gap is a single nested tool call — bounded by the
/// thirty-second HTTP timeout and the short waits in `retry`. Sixty seconds
/// is also what the budget can afford: caught within one `STALL_CHECK` of
/// that, a stall costs a third of the hundred and eighty seconds instead of
/// the two thirds a ninety-second window would, and what is left is the
/// parent's chance to answer from the findings already gathered.
const NESTED_STALL: std::time::Duration = std::time::Duration::from_secs(60);

/// What the parent sends: everything the specialist needs, in one string,
/// because the specialist sees no history.
#[derive(Debug, serde::Deserialize)]
pub struct Brief {
    pub brief: String,
}

/// The presentation rules that apply to a set of findings.
pub type Guidance = Box<dyn Fn(&[Finding]) -> Vec<String> + Send + Sync>;

/// A nested agent offered to its parent as one tool.
pub struct Specialist<M: rig::completion::CompletionModel> {
    /// The tool name the parent calls.
    pub name: &'static str,
    /// Tells the parent when to call it and what a brief must contain.
    pub description: String,
    pub agent: rig::agent::Agent<M>,
    /// The run's sink, so nested tool calls show in the chat as progress.
    pub events: scout_api::EventSink,
    /// The outer run's pulse: touched on every nested stream item, so the
    /// outer stall guard sees this run as alive while its own stream is
    /// silent.
    pub pulse: std::sync::Arc<crate::run::Pulse>,
    /// The presentation rules that apply to a set of findings.
    pub guidance: Guidance,
    pub budget: std::time::Duration,
}

impl<M> rig::tool::Tool for Specialist<M>
where
    M: rig::completion::CompletionModel + 'static,
    M::StreamingResponse: rig::completion::GetTokenUsage,
{
    const NAME: &'static str = "specialist";
    type Error = SpecialistError;
    type Args = Brief;
    type Output = Report;

    fn name(&self) -> String {
        self.name.to_string()
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "brief": {"type": "string", "description": "a self-contained request: everything the specialist needs, since it sees nothing else of the conversation"}
            },
            "required": ["brief"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        use futures::StreamExt;
        use rig::agent::MultiTurnStreamItem;
        use rig::completion::message::ToolResultContent;
        use rig::streaming::{StreamedUserContent, StreamingPrompt};

        let started = tokio::time::Instant::now();
        let mut collector = Collector::default();
        // No history on purpose: the brief is the whole conversation.
        let outcome: Result<Option<&'static str>, tokio::time::error::Elapsed> =
            tokio::time::timeout(self.budget, async {
                let mut stream = self.agent.stream_prompt(args.brief.as_str()).await;
                // When *this* stream last produced anything. `self.pulse`
                // cannot answer that and must not be asked: it is the outer
                // run's, touched below precisely so the parent counts this
                // run as alive, so a nested stream that has died keeps it
                // fresh while it burns the whole budget in silence. That is
                // what a cut-short flight search cost in production — a
                // hundred and twenty seconds indistinguishable from work.
                let mut last_item = started;
                loop {
                    let next = loop {
                        match tokio::time::timeout(crate::run::STALL_CHECK, stream.next()).await {
                            Ok(Some(item)) => {
                                self.pulse.touch();
                                last_item = tokio::time::Instant::now();
                                break item;
                            }
                            Ok(None) => return Some("the specialist ended without answering"),
                            Err(_) if last_item.elapsed() < NESTED_STALL => continue,
                            Err(_) => return Some("it stopped responding"),
                        }
                    };
                    match next {
                        Ok(MultiTurnStreamItem::ToolExecutionStart { tool_call, internal_call_id }) => {
                            collector.tool_started(
                                &internal_call_id,
                                &tool_call.function.name,
                                tool_call.function.arguments,
                                &self.events,
                            );
                        }
                        Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                            tool_result,
                            internal_call_id,
                        })) => {
                            let text = tool_result
                                .content
                                .iter()
                                .filter_map(|c| match c {
                                    ToolResultContent::Text(t) => Some(t.text.as_str()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("");
                            collector.tool_finished(&internal_call_id, &text, &self.events);
                        }
                        Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                            collector.finished(res.output());
                            return None;
                        }
                        Ok(_) => {}
                        Err(e) if crate::run::is_max_turns(&e) => {
                            return Some("it ran out of research steps");
                        }
                        Err(e) => {
                            tracing::warn!(specialist = self.name, error = %e, "the nested run failed");
                            return Some("the model call failed");
                        }
                    }
                }
            })
            .await;
        let cut_short = match outcome {
            Ok(reason) => reason,
            Err(_) => Some("it took too long"),
        };
        collector.report(cut_short, &*self.guidance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn no_guidance(_: &[Finding]) -> Vec<String> {
        Vec::new()
    }

    /// Every progress line the chat has been given since the last look, in
    /// the order it would show them - which is what a reader watching one
    /// status line actually experiences.
    fn lines(
        seen: &mut tokio::sync::mpsc::UnboundedReceiver<scout_api::AgentEvent>,
    ) -> Vec<String> {
        let mut shown = Vec::new();
        while let Ok(event) = seen.try_recv() {
            match event {
                scout_api::AgentEvent::Tool(text) => shown.push(text),
                other => panic!("this file emits progress and nothing else, got {other:?}"),
            }
        }
        shown
    }

    #[test]
    fn the_chat_hears_the_model_is_working_only_once_no_call_is_left_running() {
        // Two calls out at once: the first result leaves a line that is
        // still true, so the working line has to wait for the second.
        let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let mut c = Collector::default();

        c.tool_started("c1", "search_flights", json!({"origin": "AMS", "destination": "LIS"}), &events);
        c.tool_started("c2", "search_flights", json!({"origin": "AMS", "destination": "OPO"}), &events);
        c.tool_finished("c1", r#"{"found":1}"#, &events);

        assert_eq!(
            lines(&mut seen),
            vec!["✈️ searching AMS→LIS", "✈️ searching AMS→OPO"],
            "a search still running says more than a line about working"
        );

        c.tool_finished("c2", r#"{"found":2}"#, &events);

        assert_eq!(lines(&mut seen), vec![WORKING], "and only then, once");
    }

    #[test]
    fn a_tool_start_is_told_to_the_chat_and_its_result_becomes_a_finding() {
        let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let mut c = Collector::default();

        c.tool_started("c1", "search_flights", json!({"origin": "AMS", "destination": "LIS", "departure_date": "2026-10-12"}), &events);
        c.tool_finished("c1", r#"{"route":"AMS-LIS","found":3}"#, &events);

        match seen.try_recv() {
            Ok(scout_api::AgentEvent::Tool(text)) => assert_eq!(text, "✈️ searching AMS→LIS 2026-10-12"),
            other => panic!("expected a progress line, got {other:?}"),
        }
        assert_eq!(
            c.findings,
            vec![Finding {
                tool: "search_flights".to_string(),
                args: json!({"origin": "AMS", "destination": "LIS", "departure_date": "2026-10-12"}),
                output: json!({"route": "AMS-LIS", "found": 3}),
                failed: false,
            }]
        );
    }

    #[test]
    fn a_result_that_is_not_json_is_a_failed_finding() {
        let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
        let mut c = Collector::default();
        c.tool_started("c1", "show_trip", json!({}), &events);
        c.tool_finished("c1", "no trip called Lisbon", &events);
        assert!(c.findings[0].failed);
        assert_eq!(c.findings[0].output, json!("no trip called Lisbon"));
    }

    #[test]
    fn a_run_whose_only_call_failed_has_collected_nothing() {
        let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
        let mut c = Collector::default();
        c.tool_started("c1", "search_flights", json!({}), &events);
        c.tool_finished("c1", "duffel api error (status 429): slow down", &events);
        assert!(c.report(Some("the model call failed"), &no_guidance).is_err());
    }

    #[test]
    fn a_result_for_a_call_never_started_is_ignored() {
        let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let mut c = Collector::default();
        c.tool_finished("ghost", "{}", &events);
        assert!(c.findings.is_empty());
        assert!(seen.try_recv().is_err(), "a result nobody was waiting for is not progress");
    }

    #[test]
    fn the_report_carries_the_summary_the_findings_and_the_guidance() {
        let mut c = Collector::default();
        let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
        c.tool_started("c1", "search_flights", json!({}), &events);
        c.tool_finished("c1", r#"{"found":0}"#, &events);
        c.finished("<think>hmm</think>Searched AMS to LIS on the 12th.");

        let report = c.report(None, &|f: &[Finding]| vec![format!("{} findings", f.len())]).unwrap();
        assert_eq!(report.summary, "Searched AMS to LIS on the 12th.");
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.guidance, vec!["1 findings"]);
    }

    #[test]
    fn a_failure_after_a_finding_still_returns_the_finding() {
        // Two routes paid for before the third failed are still worth
        // handing back.
        let mut c = Collector::default();
        let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
        c.tool_started("c1", "search_flights", json!({}), &events);
        c.tool_finished("c1", r#"{"found":2}"#, &events);

        let report = c.report(Some("the model stopped responding"), &no_guidance).unwrap();
        assert_eq!(report.findings.len(), 1);
        assert!(
            report.summary.contains("the model stopped responding"),
            "the parent must be told what went wrong: {}",
            report.summary
        );
    }

    #[test]
    fn a_tool_call_written_as_prose_is_not_an_answer() {
        // The nested model wrote the call out as text; rig took that for
        // the answer, so the stream ended cleanly with nothing searched.
        let mut c = Collector::default();
        c.finished(
            "<tool_call>\n<invoke name=\"search_flights\"><origin>AMS</origin><destination>LIS</destination></invoke>\n</tool_call>",
        );
        let err = c.report(None, &no_guidance).unwrap_err();
        assert!(err.to_string().contains("tool call as text"), "got: {err}");
    }

    #[test]
    fn a_plain_answer_with_nothing_searched_is_still_an_answer() {
        // The desk may legitimately answer from the brief alone: "nothing
        // to search, the date is missing".
        let mut c = Collector::default();
        c.finished("The brief gives no date, so nothing was searched.");
        let report = c.report(None, &no_guidance).unwrap();
        assert_eq!(report.summary, "The brief gives no date, so nothing was searched.");
        assert!(report.findings.is_empty());
    }

    #[test]
    fn a_failure_with_nothing_collected_is_an_error() {
        let c = Collector::default();
        let err = c.report(Some("this took too long"), &no_guidance).unwrap_err();
        assert_eq!(err.to_string(), "the specialist could not answer: this took too long");
    }

    /// A stand-in for any Scout tool: returns a struct, so JSON.
    struct Probe;
    #[derive(serde::Deserialize)]
    struct ProbeArgs { q: i64 }
    #[derive(serde::Serialize)]
    struct ProbeOut { ok: bool, q: i64 }
    #[derive(Debug, thiserror::Error)]
    #[error("probe broke")]
    struct ProbeError;
    impl rig::tool::Tool for Probe {
        const NAME: &'static str = "probe";
        type Error = ProbeError;
        type Args = ProbeArgs;
        type Output = ProbeOut;
        fn description(&self) -> String { "a probe".to_string() }
        fn parameters(&self) -> serde_json::Value { json!({"type": "object", "properties": {"q": {"type": "integer"}}}) }
        async fn call(&self, a: ProbeArgs) -> Result<ProbeOut, ProbeError> {
            if a.q < 0 { Err(ProbeError) } else { Ok(ProbeOut { ok: true, q: a.q }) }
        }
    }

    /// A scripted model: one turn calling `probe` with `q`, then a text turn.
    fn scripted(q: i64, turns: usize) -> rig::agent::Agent<rig::test_utils::MockCompletionModel> {
        use rig::test_utils::{MockCompletionModel, MockStreamEvent};
        let call = vec![
            MockStreamEvent::tool_call("t1", "probe", json!({"q": q})),
            MockStreamEvent::final_response_with_default_usage(),
        ];
        let answer = vec![
            MockStreamEvent::text("<think>ok</think>Probed once."),
            MockStreamEvent::final_response_with_default_usage(),
        ];
        let model = MockCompletionModel::from_stream_turns([call, answer]);
        rig::agent::AgentBuilder::new(model).preamble("test").tool(Probe).default_max_turns(turns).build()
    }

    /// A scripted model calling `probe` once per entry in `qs`, each call in
    /// its own turn, then answering - so a test can watch what the chat is
    /// told between one round of tools and the next.
    fn scripted_rounds(qs: &[i64]) -> rig::agent::Agent<rig::test_utils::MockCompletionModel> {
        use rig::test_utils::{MockCompletionModel, MockStreamEvent};
        let mut turns: Vec<Vec<MockStreamEvent>> = qs
            .iter()
            .enumerate()
            .map(|(n, q)| {
                vec![
                    MockStreamEvent::tool_call(format!("t{n}"), "probe", json!({"q": q})),
                    MockStreamEvent::final_response_with_default_usage(),
                ]
            })
            .collect();
        turns.push(vec![
            MockStreamEvent::text("Probed a few times."),
            MockStreamEvent::final_response_with_default_usage(),
        ]);
        let turns_allowed = turns.len() + 1;
        let model = MockCompletionModel::from_stream_turns(turns);
        rig::agent::AgentBuilder::new(model)
            .preamble("test")
            .tool(Probe)
            .default_max_turns(turns_allowed)
            .build()
    }

    /// A tool that takes exactly as long as it is told to. rig surfaces a
    /// call's start and its result together, once the tool has returned, so
    /// this is how a test makes the nested stream silent for a chosen
    /// stretch - the shape a stalled provider has from the outside.
    struct Wait;
    #[derive(serde::Deserialize)]
    struct WaitArgs { secs: u64 }
    #[derive(serde::Serialize)]
    struct WaitOut { waited: u64 }
    impl rig::tool::Tool for Wait {
        const NAME: &'static str = "wait";
        type Error = ProbeError;
        type Args = WaitArgs;
        type Output = WaitOut;
        fn description(&self) -> String { "waits".to_string() }
        fn parameters(&self) -> serde_json::Value { json!({"type": "object", "properties": {"secs": {"type": "integer"}}}) }
        async fn call(&self, a: WaitArgs) -> Result<WaitOut, ProbeError> {
            tokio::time::sleep(std::time::Duration::from_secs(a.secs)).await;
            Ok(WaitOut { waited: a.secs })
        }
    }

    /// A scripted model calling `wait` once per entry in `secs`, each in its
    /// own turn, then answering.
    fn scripted_waits(secs: &[u64]) -> rig::agent::Agent<rig::test_utils::MockCompletionModel> {
        use rig::test_utils::{MockCompletionModel, MockStreamEvent};
        let mut turns: Vec<Vec<MockStreamEvent>> = secs
            .iter()
            .enumerate()
            .map(|(n, s)| {
                vec![
                    MockStreamEvent::tool_call(format!("w{n}"), "wait", json!({"secs": s})),
                    MockStreamEvent::final_response_with_default_usage(),
                ]
            })
            .collect();
        turns.push(vec![
            MockStreamEvent::text("Waited."),
            MockStreamEvent::final_response_with_default_usage(),
        ]);
        let turns_allowed = turns.len() + 1;
        let model = MockCompletionModel::from_stream_turns(turns);
        rig::agent::AgentBuilder::new(model)
            .preamble("test")
            .tool(Wait)
            .default_max_turns(turns_allowed)
            .build()
    }

    fn specialist<M: rig::completion::CompletionModel>(
        agent: rig::agent::Agent<M>,
        events: scout_api::EventSink,
    ) -> Specialist<M> {
        specialist_with_pulse(agent, events, crate::run::Pulse::default())
    }

    fn specialist_with_pulse<M: rig::completion::CompletionModel>(
        agent: rig::agent::Agent<M>,
        events: scout_api::EventSink,
        pulse: crate::run::Pulse,
    ) -> Specialist<M> {
        Specialist {
            name: "ask_probe",
            description: "test".to_string(),
            agent,
            events,
            pulse: std::sync::Arc::new(pulse),
            guidance: Box::new(|f: &[Finding]| vec![format!("{} ok", f.iter().filter(|x| !x.failed).count())]),
            budget: std::time::Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn a_nested_run_becomes_a_report_of_what_its_tools_returned() {
        let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let stale = crate::run::Pulse::aged(std::time::Duration::from_secs(60));
        let tool = specialist_with_pulse(scripted(7, 5), events, stale);

        let report = rig::tool::Tool::call(&tool, Brief { brief: "probe seven".to_string() }).await.unwrap();

        assert_eq!(report.summary, "Probed once.");
        assert_eq!(
            report.findings,
            vec![Finding { tool: "probe".to_string(), args: json!({"q": 7}), output: json!({"ok": true, "q": 7}), failed: false }]
        );
        assert_eq!(report.guidance, vec!["1 ok"]);
        match seen.try_recv() {
            Ok(scout_api::AgentEvent::Tool(text)) => assert_eq!(text, "⚙️ probe"),
            other => panic!("the chat must see the nested call: {other:?}"),
        }
        assert!(tool.pulse.since() < std::time::Duration::from_secs(1), "the pulse was touched");
    }

    #[tokio::test]
    async fn every_round_of_nested_calls_hands_the_chat_back_a_true_line() {
        // Two rounds, so the sequence shows what the reader sees: a tool
        // line while the tool runs, the working line the moment it is
        // answered, and the next tool line taking that one's place.
        let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let tool = specialist(scripted_rounds(&[1, 2]), events);

        let report = rig::tool::Tool::call(&tool, Brief { brief: "probe twice".to_string() }).await.unwrap();

        assert_eq!(report.findings.len(), 2);
        assert_eq!(lines(&mut seen), vec!["⚙️ probe", WORKING, "⚙️ probe", WORKING]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_nested_stream_that_goes_quiet_is_reported_as_a_stall_not_as_a_slow_run() {
        // Nothing at all comes back from the nested run. Before the guard
        // this was indistinguishable from work and cost the whole budget.
        let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
        let mut tool = specialist(scripted_waits(&[600]), events);
        tool.budget = SPECIALIST_BUDGET;
        let started = tokio::time::Instant::now();

        let err = rig::tool::Tool::call(&tool, Brief { brief: "go quiet".to_string() }).await.unwrap_err();

        assert_eq!(err.to_string(), "the specialist could not answer: it stopped responding");
        assert!(
            started.elapsed() < NESTED_STALL + crate::run::STALL_CHECK * 2,
            "the guard ended it, not the budget: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_nested_run_that_answers_slowly_but_steadily_is_left_to_finish() {
        // Three calls, each taking most of a stall window: far longer in
        // total than the window, never silent for one of them. A guard that
        // killed this would be a worse bug than the one it fixes.
        let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
        let mut tool = specialist(scripted_waits(&[50, 50, 50]), events);
        tool.budget = SPECIALIST_BUDGET;

        let report = rig::tool::Tool::call(&tool, Brief { brief: "wait thrice".to_string() }).await.unwrap();

        assert_eq!(report.summary, "Waited.", "no cut-short reason belongs in this summary");
        assert_eq!(report.findings.len(), 3);
    }

    #[tokio::test]
    async fn a_nested_tool_error_is_a_failed_finding_not_a_result() {
        let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
        let tool = specialist(scripted(-1, 5), events);
        let report = rig::tool::Tool::call(&tool, Brief { brief: "break".to_string() }).await.unwrap();
        assert!(report.findings[0].failed);
        assert_eq!(report.findings[0].output, json!("probe broke"));
        assert_eq!(report.guidance, vec!["0 ok"]);
    }

    #[tokio::test]
    async fn running_out_of_turns_after_a_finding_still_reports_it() {
        let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
        let tool = specialist(scripted(1, 1), events);
        let report = rig::tool::Tool::call(&tool, Brief { brief: "one turn".to_string() }).await.unwrap();
        assert_eq!(report.findings.len(), 1);
        assert!(report.summary.contains("ran out of research steps"), "got: {}", report.summary);
    }

    #[tokio::test]
    async fn a_nested_run_that_cannot_reach_the_model_fails_with_a_sentence() {
        // The model endpoint is a closed port, so the nested run dies on
        // its first call with nothing collected. That is the one case
        // where the tool errors instead of reporting.
        use rig::client::CompletionClient;
        let llm = crate::agent::llm_client("k", "http://127.0.0.1:1").unwrap();
        let agent = llm.agent(crate::agent::MODEL).preamble("test").default_max_turns(2).build();
        let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
        let tool = specialist(agent, events);

        let err = rig::tool::Tool::call(&tool, Brief { brief: "anything".to_string() }).await.unwrap_err();
        assert!(
            err.to_string().starts_with("the specialist could not answer:"),
            "got: {err}"
        );
        assert_eq!(rig::tool::Tool::name(&tool), "ask_probe");
    }
}
