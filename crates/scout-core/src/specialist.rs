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
    pub(crate) fn tool_finished(&mut self, call_id: &str, text: &str) {
        let Some((tool, args)) = self.pending.remove(call_id) else {
            return;
        };
        let (output, failed) = match serde_json::from_str(text) {
            Ok(v) => (v, false),
            Err(_) => (serde_json::Value::String(text.to_string()), true),
        };
        self.findings.push(Finding { tool, args, output, failed });
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

        let mut collector = Collector::default();
        // No history on purpose: the brief is the whole conversation.
        let outcome: Result<Option<&'static str>, tokio::time::error::Elapsed> =
            tokio::time::timeout(self.budget, async {
                let mut stream = self.agent.stream_prompt(args.brief.as_str()).await;
                while let Some(item) = stream.next().await {
                    self.pulse.touch();
                    match item {
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
                            collector.tool_finished(&internal_call_id, &text);
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
                Some("the specialist ended without answering")
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

    #[test]
    fn a_tool_start_is_told_to_the_chat_and_its_result_becomes_a_finding() {
        let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let mut c = Collector::default();

        c.tool_started("c1", "search_flights", json!({"origin": "AMS", "destination": "LIS", "departure_date": "2026-10-12"}), &events);
        c.tool_finished("c1", r#"{"route":"AMS-LIS","found":3}"#);

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
        c.tool_finished("c1", "no trip called Lisbon");
        assert!(c.findings[0].failed);
        assert_eq!(c.findings[0].output, json!("no trip called Lisbon"));
    }

    #[test]
    fn a_run_whose_only_call_failed_has_collected_nothing() {
        let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
        let mut c = Collector::default();
        c.tool_started("c1", "search_flights", json!({}), &events);
        c.tool_finished("c1", "duffel api error (status 429): slow down");
        assert!(c.report(Some("the model call failed"), &no_guidance).is_err());
    }

    #[test]
    fn a_result_for_a_call_never_started_is_ignored() {
        let mut c = Collector::default();
        c.tool_finished("ghost", "{}");
        assert!(c.findings.is_empty());
    }

    #[test]
    fn the_report_carries_the_summary_the_findings_and_the_guidance() {
        let mut c = Collector::default();
        let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
        c.tool_started("c1", "search_flights", json!({}), &events);
        c.tool_finished("c1", r#"{"found":0}"#);
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
        c.tool_finished("c1", r#"{"found":2}"#);

        let report = c.report(Some("the model stopped responding"), &no_guidance).unwrap();
        assert_eq!(report.findings.len(), 1);
        assert!(
            report.summary.contains("the model stopped responding"),
            "the parent must be told what went wrong: {}",
            report.summary
        );
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

    fn specialist<M: rig::completion::CompletionModel>(
        agent: rig::agent::Agent<M>,
        events: scout_api::EventSink,
    ) -> Specialist<M> {
        Specialist {
            name: "ask_probe",
            description: "test".to_string(),
            agent,
            events,
            pulse: std::sync::Arc::new(crate::run::Pulse::default()),
            guidance: Box::new(|f: &[Finding]| vec![format!("{} ok", f.iter().filter(|x| !x.failed).count())]),
            budget: std::time::Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn a_nested_run_becomes_a_report_of_what_its_tools_returned() {
        let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let tool = specialist(scripted(7, 5), events);

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
