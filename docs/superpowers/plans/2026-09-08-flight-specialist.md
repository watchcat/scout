# Flight Specialist Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move every flight and trip tool out of the main agent into a flight agent that the main agent calls through one tool, `ask_flights`, whose result carries the findings and the rules for presenting them.

**Architecture:** A generic `Specialist` (a rig `Tool` holding a nested rig `Agent`) streams the nested run, forwards nested tool starts to the chat as progress, collects each nested tool's output as a `Finding`, and returns a `Report { summary, findings, guidance }`. `flights.rs` builds the flight agent (its own preamble, the moved tools) and supplies the guidance function. The main preamble loses its flight half and gains one rule.

**Tech Stack:** Rust, rig 0.40 (`rig::agent::MultiTurnStreamItem`, `rig::streaming::StreamingPrompt`), tokio, serde_json, thiserror. Spec: `docs/superpowers/specs/2026-09-08-flight-specialist-design.md`.

**Repo rules:** work on a branch in the main checkout (not a worktree, to reuse the DuckDB build cache). Do NOT run `cargo fmt`. Tests are run with `cargo test -p scout-core <filter>`. Commits end with `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`. Watch each new test fail before making it pass.

---

## File structure

| File | Responsibility |
|---|---|
| `crates/scout-core/src/specialist.rs` (new) | The generic wrapper: `Collector`, `Finding`, `Report`, `Specialist` tool, `SPECIALIST_BUDGET` |
| `crates/scout-core/src/flights.rs` (new) | `FLIGHT_PREAMBLE`, `FLIGHT_TURNS`, `guidance()`, `build_flight_agent()`, `ask_flights()` |
| `crates/scout-core/src/agent.rs` | Main preamble shrinks; `ALL_TOOLS`, `available_tools`, `build_agent(.., events)`; helpers made `pub(crate)` |
| `crates/scout-core/src/run.rs` | Passes the sink into `build_agent`; `is_max_turns` becomes `pub(crate)` |
| `crates/scout-core/src/config.rs` | `FLIGHT_MODEL` |
| `crates/scout-core/src/core.rs` | `AgentDeps.flight_model` |
| `crates/scout-core/src/describe.rs` | Progress sentences for the flight and trip tools |
| `crates/scout-core/src/lib.rs` | Registers the two modules |
| `README.md`, `.env.example`, `docs/BOARD.md` | Docs |

---

### Task 0: Branch

- [ ] **Step 1: Branch from main**

```bash
cd /Users/watchcat/work/rust/scout && git checkout main && git pull --ff-only && git checkout -b feat/flight-specialist
```

---

### Task 1: The collector and the report (pure half of `specialist.rs`)

**Files:**
- Create: `crates/scout-core/src/specialist.rs`
- Modify: `crates/scout-core/src/lib.rs`

- [ ] **Step 1: Register the module and write the failing tests**

Add to `crates/scout-core/src/lib.rs` after `mod agent;`:

```rust
mod flights;
mod specialist;
```

(`flights` is created in Task 3; until then add only `mod specialist;` and add `mod flights;` in Task 3.)

Create `crates/scout-core/src/specialist.rs` with the tests only:

```rust
//! A specialist agent the main agent calls as a tool.
//!
//! The wrapper knows nothing about flights. It streams a nested run, tells
//! the chat what the nested tools are doing, keeps every nested tool's real
//! output, and hands the parent a report: the specialist's own sentence,
//! the findings, and whatever presentation rules the instance says apply
//! to those findings. Numbers reach the parent through the findings, never
//! through the nested model's prose.

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
            }]
        );
    }

    #[test]
    fn a_result_that_is_not_json_is_kept_as_text() {
        let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
        let mut c = Collector::default();
        c.tool_started("c1", "show_trip", json!({}), &events);
        c.tool_finished("c1", "no trip called Lisbon");
        assert_eq!(c.findings[0].output, json!("no trip called Lisbon"));
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
}
```

- [ ] **Step 2: Run the tests to watch them fail**

Run: `cargo test -p scout-core specialist::`
Expected: compile error, `Collector`, `Finding`, `SpecialistError` not found.

- [ ] **Step 3: Write the pure half**

Insert above the `#[cfg(test)]` block in `crates/scout-core/src/specialist.rs`:

```rust
use std::collections::HashMap;

/// One nested tool call and what it returned, as the tool returned it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Finding {
    pub tool: String,
    pub args: serde_json::Value,
    /// The tool's own JSON, or its text when it did not return JSON.
    pub output: serde_json::Value,
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
        let output = serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.to_string()));
        self.findings.push(Finding { tool, args, output });
    }

    /// The nested model's final text.
    pub(crate) fn finished(&mut self, output: &str) {
        self.summary = crate::text::strip_thinking(output);
    }

    /// The report, or an error when the run was cut short with nothing to
    /// show. `cut_short` is the reason when the run did not finish on its
    /// own; it is written into the summary so the parent knows.
    pub(crate) fn report(
        self,
        cut_short: Option<&str>,
        guidance: &dyn Fn(&[Finding]) -> Vec<String>,
    ) -> Result<Report, SpecialistError> {
        let summary = match cut_short {
            None => self.summary,
            Some(reason) if self.findings.is_empty() => {
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
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p scout-core specialist::`
Expected: 5 pass, 1 fails: `a_tool_start_is_told_to_the_chat_and_its_result_becomes_a_finding` gets `⚙️ search_flights` instead of `✈️ searching AMS→LIS 2026-10-12`. That is Task 2's job; leave it red.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/specialist.rs crates/scout-core/src/lib.rs
git commit -m "feat(specialist): collect a nested run into a report

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 2: Progress sentences for the flight and trip tools

**Files:**
- Modify: `crates/scout-core/src/describe.rs`

- [ ] **Step 1: Write the failing test**

Add inside `mod tests` in `crates/scout-core/src/describe.rs`:

```rust
    #[test]
    fn flight_and_trip_calls_read_as_progress_not_as_tool_names() {
        // These used to fall through to "⚙️ <name>". Now that they run
        // inside the flight specialist, they are the only progress the
        // chat sees during a flight question.
        assert_eq!(
            describe("ask_flights", &json!({"brief": "return AMS-LIS 12-19 Oct, 2 adults"})),
            "✈️ asking the flight desk: return AMS-LIS 12-19 Oct, 2 adults"
        );
        assert_eq!(describe("ask_flights", &json!({})), "✈️ asking the flight desk");
        assert_eq!(
            describe("search_flights", &json!({"origin": "AMS", "destination": "LIS", "departure_date": "2026-10-12"})),
            "✈️ searching AMS→LIS 2026-10-12"
        );
        assert_eq!(
            describe("search_flights", &json!({"origin": "AMS", "destination": "LIS", "departure_date": "2026-10-12", "flex_days": 2})),
            "✈️ searching AMS→LIS 2026-10-12 ±2"
        );
        assert_eq!(describe("search_flights", &json!({})), "✈️ searching flights");
        assert_eq!(describe("flight_booking_links", &json!({"ignav_id": "x"})), "🔗 fetching booking links");
        assert_eq!(describe("create_booking_link", &json!({})), "🔗 fetching booking links");
        assert_eq!(describe("finalise_trip", &json!({"trip": "Lisbon"})), "✈️ pricing the trip");
        assert_eq!(describe("show_trip", &json!({})), "🗺️ reading the trip");
        for tool in ["add_trip_segment", "add_trip_option", "choose_trip_option", "update_trip_segment", "drop_trip_segment", "delete_trip"] {
            assert_eq!(describe(tool, &json!({})), "🗺️ updating the trip", "{tool}");
        }
    }
```

- [ ] **Step 2: Run it to watch it fail**

Run: `cargo test -p scout-core describe::`
Expected: FAIL, left `⚙️ ask_flights`.

- [ ] **Step 3: Add the arms**

In `describe()` in `crates/scout-core/src/describe.rs`, before `other => format!("⚙️ {other}"),`:

```rust
        "ask_flights" => match s("brief") {
            b if b.is_empty() => "✈️ asking the flight desk".to_string(),
            b => format!("✈️ asking the flight desk: {b}"),
        },
        "search_flights" => {
            let (from, to, day) = (s("origin"), s("destination"), s("departure_date"));
            if from.is_empty() || to.is_empty() {
                return "✈️ searching flights".to_string();
            }
            let flex = args.get("flex_days").and_then(|v| v.as_u64()).filter(|n| *n > 0);
            match flex {
                Some(n) => format!("✈️ searching {from}→{to} {day} ±{n}"),
                None => format!("✈️ searching {from}→{to} {day}"),
            }
        }
        "flight_booking_links" | "create_booking_link" => "🔗 fetching booking links".to_string(),
        "finalise_trip" => "✈️ pricing the trip".to_string(),
        "show_trip" => "🗺️ reading the trip".to_string(),
        "add_trip_segment" | "add_trip_option" | "choose_trip_option" | "update_trip_segment"
        | "drop_trip_segment" | "delete_trip" => "🗺️ updating the trip".to_string(),
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p scout-core describe:: specialist::`
Expected: all pass, including Task 1's red test.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/describe.rs
git commit -m "feat(describe): flight and trip calls read as progress

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 3: The `Specialist` tool (streaming half of `specialist.rs`)

**Files:**
- Modify: `crates/scout-core/src/specialist.rs`
- Modify: `crates/scout-core/src/run.rs:483` (`is_max_turns` becomes `pub(crate)`)

- [ ] **Step 1: Write the failing test**

Add inside `mod tests` in `crates/scout-core/src/specialist.rs`:

```rust
    #[tokio::test]
    async fn a_nested_run_that_cannot_reach_the_model_fails_with_a_sentence() {
        // The model endpoint is a closed port, so the nested run dies on
        // its first call with nothing collected. That is the one case
        // where the tool errors instead of reporting.
        use rig::client::CompletionClient;
        let llm = crate::agent::llm_client("k", "http://127.0.0.1:1").unwrap();
        let agent = llm.agent(crate::agent::MODEL).preamble("test").default_max_turns(2).build();
        let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
        let tool = Specialist {
            name: "ask_nothing",
            description: "a test specialist".to_string(),
            agent,
            events,
            guidance: Box::new(no_guidance),
            budget: std::time::Duration::from_secs(5),
        };

        let err = rig::tool::Tool::call(&tool, Brief { brief: "anything".to_string() }).await.unwrap_err();
        assert!(
            err.to_string().starts_with("the specialist could not answer:"),
            "got: {err}"
        );
        assert_eq!(rig::tool::Tool::name(&tool), "ask_nothing");
    }
```

- [ ] **Step 2: Run it to watch it fail**

Run: `cargo test -p scout-core specialist::`
Expected: compile error, `Specialist` and `Brief` not found.

- [ ] **Step 3: Make `is_max_turns` reachable**

In `crates/scout-core/src/run.rs` change `fn is_max_turns(` to `pub(crate) fn is_max_turns(`.

- [ ] **Step 4: Write the tool**

Insert into `crates/scout-core/src/specialist.rs`, after the `Collector` impl and before the tests:

```rust
/// How long one nested run may take. Inside the outer run's own budget,
/// and long enough for a flexible-window search that fans out over a week.
pub const SPECIALIST_BUDGET: std::time::Duration = std::time::Duration::from_secs(180);

/// What the parent sends: everything the specialist needs, in one string,
/// because the specialist sees no history.
#[derive(Debug, serde::Deserialize)]
pub struct Brief {
    pub brief: String,
}

/// A nested agent offered to its parent as one tool.
pub struct Specialist {
    /// The tool name the parent calls.
    pub name: &'static str,
    /// Tells the parent when to call it and what a brief must contain.
    pub description: String,
    pub agent: rig::agent::Agent<rig::providers::openai::completion::CompletionModel>,
    /// The run's sink, so nested tool calls show in the chat as progress.
    pub events: scout_api::EventSink,
    /// The presentation rules that apply to a set of findings.
    pub guidance: Box<dyn Fn(&[Finding]) -> Vec<String> + Send + Sync>,
    pub budget: std::time::Duration,
}

impl rig::tool::Tool for Specialist {
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
                    match item {
                        Ok(MultiTurnStreamItem::ToolExecutionStart { tool_call, internal_call_id }) => {
                            collector.tool_started(
                                &internal_call_id,
                                &tool_call.function.name,
                                tool_call.function.arguments.clone(),
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
                Some("the model stopped responding")
            })
            .await;
        let cut_short = match outcome {
            Ok(reason) => reason,
            Err(_) => Some("it took too long"),
        };
        collector.report(cut_short, &*self.guidance)
    }
}
```

If the compiler rejects `stream_prompt(...).await` because the request type needs the history form, use `rig::streaming::StreamingChat` and `self.agent.stream_chat(args.brief.as_str(), Vec::<rig::completion::Message>::new()).await` instead; run.rs already uses that form.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p scout-core specialist::`
Expected: all 7 pass. The closed-port test finishes in well under a second.

- [ ] **Step 6: Commit**

```bash
git add crates/scout-core/src/specialist.rs crates/scout-core/src/run.rs
git commit -m "feat(specialist): a nested agent offered to its parent as one tool

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 3b: Review fixes for the specialist

Findings from the code review of Tasks 1-3. Three real gaps and some polish.

**Files:**
- Modify: `crates/scout-core/src/specialist.rs`
- Modify: `crates/scout-core/src/run.rs`
- Modify: `crates/scout-core/src/describe.rs`
- Modify: `crates/scout-core/Cargo.toml` (dev-dependency feature)

#### 3b.1 The outer stall guard must see nested activity (`Pulse`)

rig yields nothing to the outer stream until a tool returns, so while `ask_flights` runs the outer `timeout(STREAM_STALL, stream.next())` in `run.rs` sees silence, and a nested run over 90 s gets killed as a stall. Progress events go to the sink, not through the stream, so they do not help.

- [ ] **Step 1: Write the failing tests** in `run.rs` `mod tests`:

```rust
    #[tokio::test(start_paused = true)]
    async fn the_pulse_ages_until_something_touches_it() {
        let pulse = Pulse::default();
        tokio::time::advance(std::time::Duration::from_secs(100)).await;
        assert!(pulse.since() >= std::time::Duration::from_secs(100));
        pulse.touch();
        assert!(pulse.since() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn the_stall_guard_reads_the_pulse_not_the_stream_alone() {
        // A specialist's nested run is silent on the outer stream for its
        // whole length; only the pulse knows it is alive.
        let src = include_str!("run.rs");
        let src = &src[..src.find("#[cfg(test)]").expect("the tests must come last")];
        assert!(src.contains("timeout(STALL_CHECK, stream.next())"), "the stream is polled in short checks");
        assert!(src.contains("pulse.since() < STREAM_STALL"), "and a stall is judged by the pulse");
    }
```

- [ ] **Step 2: Run** `cargo test -p scout-core run::the_pulse run::the_stall_guard` — compile error, `Pulse` not found.

- [ ] **Step 3: Implement** in `run.rs`, next to `STREAM_STALL`:

```rust
/// How often the outer loop looks up from a silent stream to ask the
/// pulse whether the run is alive. Short, so a real stall is still caught
/// within `STREAM_STALL` plus one check.
const STALL_CHECK: std::time::Duration = std::time::Duration::from_secs(15);

/// The last moment anything in this run was seen doing something.
///
/// The outer stream goes quiet for the whole of a specialist's nested run
/// — rig yields a tool's start and result together, after it returns —
/// so a stall guard on the stream alone would kill every flight question
/// that takes longer than `STREAM_STALL`. The outer loop touches this on
/// every stream item and the specialist on every nested one; the guard
/// asks how long ago that was.
#[derive(Debug)]
pub(crate) struct Pulse(std::sync::Mutex<tokio::time::Instant>);

impl Default for Pulse {
    fn default() -> Self {
        Self(std::sync::Mutex::new(tokio::time::Instant::now()))
    }
}

impl Pulse {
    pub(crate) fn touch(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = tokio::time::Instant::now();
    }

    pub(crate) fn since(&self) -> std::time::Duration {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).elapsed()
    }
}
```

In `run_agent`, before `let agent = build_agent(...)`:

```rust
    let pulse = std::sync::Arc::new(Pulse::default());
```

Replace the stall guard block

```rust
                let next = match tokio::time::timeout(STREAM_STALL, stream.next()).await {
                    Ok(Some(item)) => item,
                    Ok(None) => return Ok(None),
                    Err(_) => return Ok(Some("the model stopped responding")),
                };
```

with

```rust
                // A silent stream is a stall only when nothing else in the
                // run is alive either: a specialist's nested run is silent
                // here for its whole length and reports through the pulse.
                let next = loop {
                    match tokio::time::timeout(STALL_CHECK, stream.next()).await {
                        Ok(Some(item)) => {
                            pulse.touch();
                            break item;
                        }
                        Ok(None) => return Ok(None),
                        Err(_) if pulse.since() < STREAM_STALL => continue,
                        Err(_) => return Ok(Some("the model stopped responding")),
                    }
                };
```

Update the `STREAM_STALL` doc comment: it now bounds the age of the pulse, not the gap between stream items. (`build_agent` does not take the pulse yet; Task 7 passes `pulse.clone()`.)

- [ ] **Step 4: Run** `cargo test -p scout-core run::` — all pass.

#### 3b.2 A failed nested tool is a failed finding

rig hands a tool's `Err` to the model as the error's text (`rig-core-0.40.0/src/agent/runner.rs`, test `handled_failure_delivers_model_output_and_error_outcome`). Every Scout tool returns a serialised struct, so a result that is not JSON is a failure. Today it becomes an indistinguishable `Finding` and guidance would attach search rules to an error string.

- [ ] **Step 5: Change the tests** in `specialist.rs`: rename `a_result_that_is_not_json_is_kept_as_text` to `a_result_that_is_not_json_is_a_failed_finding` and assert `c.findings[0].failed` and `c.findings[0].output == json!("no trip called Lisbon")`; add `failed: false` to the expected `Finding` in `a_tool_start_is_told_to_the_chat_and_its_result_becomes_a_finding`; add:

```rust
    #[test]
    fn a_run_whose_only_call_failed_has_collected_nothing() {
        let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
        let mut c = Collector::default();
        c.tool_started("c1", "search_flights", json!({}), &events);
        c.tool_finished("c1", "duffel api error (status 429): slow down");
        assert!(c.report(Some("the model call failed"), &no_guidance).is_err());
    }
```

- [ ] **Step 6: Implement**: `Finding` gains `pub failed: bool` with the doc comment "True when the tool returned an error, which rig hands to the model as text; `output` is then that text. Every Scout tool returns a serialised struct, so 'not JSON' is the signal." In `tool_finished`:

```rust
        let (output, failed) = match serde_json::from_str(text) {
            Ok(v) => (v, false),
            Err(_) => (serde_json::Value::String(text.to_string()), true),
        };
        self.findings.push(Finding { tool, args, output, failed });
```

In `report`, the guard becomes `Some(reason) if self.findings.iter().all(|f| f.failed) =>` (an empty list is `all`, so the old case is covered). Add a doc line: guidance functions should count only findings with `failed == false`.

- [ ] **Step 7: Run** `cargo test -p scout-core specialist::` — all pass.

#### 3b.3 Generic over the model, and the stream loop tested end to end

- [ ] **Step 8: Dev-dependency.** In `crates/scout-core/Cargo.toml` under `[dev-dependencies]` add `rig = { version = "0.40", features = ["test-utils"] }`.

- [ ] **Step 9: Write the failing tests** in `specialist.rs`:

```rust
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
```

If the mock's error text for a failed tool is not exactly `probe broke` (rig may prefix it), read the actual text from the failing assertion and assert with `contains("probe broke")`. If `max_turns(1)` lets the answer turn through, use whatever count makes the tool run and the answer not; the assertion is on the reason.

- [ ] **Step 10: Run** `cargo test -p scout-core specialist::` — compile error: `Specialist` is not generic and has no `pulse`.

- [ ] **Step 11: Implement.** `Specialist<M: rig::completion::CompletionModel>` with `pub agent: rig::agent::Agent<M>` and `pub pulse: std::sync::Arc<crate::run::Pulse>`; the `Tool` impl becomes `impl<M> rig::tool::Tool for Specialist<M> where M: rig::completion::CompletionModel + 'static, M::StreamingResponse: rig::completion::GetTokenUsage` (copy the exact bounds rig's `StreamingPrompt` impl uses in `rig-core-0.40.0/src/agent/completion.rs`). In the stream loop, `self.pulse.touch()` on every item before matching it. Add `pub type Guidance = Box<dyn Fn(&[Finding]) -> Vec<String> + Send + Sync>;` and use it for the field and in `report`'s parameter (`&dyn Fn(...)` stays fine for `report`). Move `tool_call.function.arguments` instead of cloning. Change the end-of-stream reason to `"the specialist ended without answering"`. Update the closed-port test to construct through the `specialist()` helper (it stays a real `openai` model on the closed port).

- [ ] **Step 12: Run** `cargo test -p scout-core specialist::` and `cargo test -p scout-core run::` — all pass; `cargo clippy -p scout-core --all-targets` shows no `type_complexity`.

#### 3b.4 Polish

- [ ] **Step 13:** `describe.rs`: when `departure_date` is empty, omit the trailing space (`"✈️ searching AMS→LIS"`); add that case to the describe test.

- [ ] **Step 14: Commit**

```bash
git add crates/scout-core/src/specialist.rs crates/scout-core/src/run.rs crates/scout-core/src/describe.rs crates/scout-core/Cargo.toml
git commit -m "fix(specialist): a pulse the stall guard reads, failed findings, and the loop under test

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 4: The flight prompt and the guidance (`flights.rs`, pure half)

**Files:**
- Create: `crates/scout-core/src/flights.rs`
- Modify: `crates/scout-core/src/lib.rs` (add `mod flights;`)
- Modify: `crates/scout-core/src/agent.rs:555,613` (`rules_for_available_tools`, `percentage` become `pub(crate)`)

- [ ] **Step 1: Write the failing tests**

Create `crates/scout-core/src/flights.rs`:

```rust
//! The flight specialist: the agent that searches, books and plans trips,
//! offered to the main agent as `ask_flights`.
//!
//! Two halves. `FLIGHT_PREAMBLE` is the working half of what used to be
//! the main prompt's flight section: how to search. `guidance` is the
//! presentation half: how to write the answer, returned with the findings
//! so that only a flight turn ever pays for it.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::specialist::Finding;
    use serde_json::json;

    fn ran(tool: &str, output: serde_json::Value) -> Finding {
        Finding { tool: tool.to_string(), args: json!({}), output, failed: false }
    }

    #[test]
    fn a_failed_search_brings_no_search_rules() {
        let failed = Finding { failed: true, ..ran("search_flights", json!("duffel api error (status 429)")) };
        assert!(guidance(&[failed], 0.03).is_empty());
    }

    #[test]
    fn nothing_ran_nothing_to_say() {
        assert!(guidance(&[], 0.0).is_empty());
    }

    #[test]
    fn a_search_brings_the_option_rules_and_only_a_window_brings_the_by_date_rules() {
        let plain = guidance(&[ran("search_flights", json!({"route": "AMS-LIS", "by_date": []}))], 0.0);
        let text = plain.join("\n");
        assert!(text.contains("Cheapest, Fastest, Best balance"), "got: {text}");
        assert!(text.contains("itinerary"), "got: {text}");
        assert!(text.contains("price_status"), "got: {text}");
        assert!(text.contains("self_transfer"), "got: {text}");
        assert!(!text.contains("by_date"), "no window, no window rules: {text}");

        let window = guidance(
            &[ran("search_flights", json!({"route": "AMS-LIS ±2", "by_date": [{"date": "2026-10-11"}]}))],
            0.0,
        );
        assert!(window.join("\n").contains("by_date"), "got: {window:?}");
    }

    #[test]
    fn booking_and_trip_rules_come_with_their_tools() {
        let text = guidance(&[ran("flight_booking_links", json!({}))], 0.0).join("\n");
        assert!(text.contains("airline's link first"), "got: {text}");
        assert!(!text.contains("Cheapest, Fastest"), "no search, no option rules: {text}");

        let text = guidance(&[ran("create_booking_link", json!({}))], 0.0).join("\n");
        assert!(text.contains("single-use"), "got: {text}");

        for tool in ["add_trip_segment", "show_trip", "finalise_trip", "delete_trip"] {
            let text = guidance(&[ran(tool, json!({}))], 0.0).join("\n");
            assert!(text.contains("not_ready"), "{tool}: {text}");
            assert!(text.contains("separate tickets"), "{tool}: {text}");
        }
    }

    #[test]
    fn the_fee_is_stated_only_when_charged() {
        let free = guidance(&[ran("search_flights", json!({}))], 0.0).join("\n");
        assert!(!free.contains("booking fee"), "got: {free}");
        let charged = guidance(&[ran("search_flights", json!({}))], 0.03).join("\n");
        assert!(charged.contains("3% booking fee"), "got: {charged}");
    }

    #[test]
    fn each_section_appears_once_however_many_times_its_tool_ran() {
        let g = guidance(
            &[ran("search_flights", json!({})), ran("search_flights", json!({}))],
            0.0,
        );
        assert_eq!(g.iter().filter(|s| s.contains("Cheapest, Fastest")).count(), 1);
    }

    #[test]
    fn the_flight_prompt_searches_and_the_guidance_presents() {
        // The split is the point: a rule in the wrong half is either paid
        // for by every shopping turn or missing from every flight turn.
        for word in ["itinerary", "Cheapest, Fastest", "price_status", "self_transfer"] {
            assert!(!FLIGHT_PREAMBLE.contains(word), "{word:?} is presentation, it belongs in guidance");
        }
        for word in ["flex_days", "add_trip_segment", "IATA", "no prices"] {
            assert!(FLIGHT_PREAMBLE.contains(word), "{word:?} is missing from the flight prompt");
        }
    }

    #[test]
    fn the_booking_rules_drop_with_their_tools() {
        let none = crate::agent::rules_for_available_tools(FLIGHT_PREAMBLE, &[]);
        assert!(!none.contains("flight_booking_links"), "got: {none}");
        assert!(!none.contains("create_booking_link"), "got: {none}");
        assert!(none.contains("search_flights"), "the search rule is unconditional");

        let all = crate::agent::rules_for_available_tools(FLIGHT_PREAMBLE, FLIGHT_TOOLS);
        assert!(all.contains("flight_booking_links") && all.contains("create_booking_link"));
    }

    #[test]
    fn every_conditional_rule_in_the_flight_prompt_is_a_flight_tool() {
        for rule in FLIGHT_PREAMBLE.split("\n- ").skip(1) {
            if let Some((tool, _)) = rule.strip_prefix("When ").and_then(|r| r.split_once(" is available,")) {
                assert!(FLIGHT_TOOLS.contains(&tool), "{tool:?} is offered conditionally but nothing gates it");
            }
        }
    }
}
```

Add `mod flights;` to `crates/scout-core/src/lib.rs` next to `mod specialist;`.

- [ ] **Step 2: Run to watch them fail**

Run: `cargo test -p scout-core flights::`
Expected: compile error, `guidance`, `FLIGHT_PREAMBLE`, `FLIGHT_TOOLS` not found.

- [ ] **Step 3: Make the two agent helpers reachable**

In `crates/scout-core/src/agent.rs`: `fn rules_for_available_tools(` → `pub(crate) fn rules_for_available_tools(`; `fn percentage(` → `pub(crate) fn percentage(`.

- [ ] **Step 4: Write the prompt, the tool list and the guidance**

Insert above the tests in `crates/scout-core/src/flights.rs`:

```rust
/// Model calls one flight question may take. A flexible return with a
/// booking link is search, links, answer: three. Twelve leaves room for a
/// multi-city plan built segment by segment.
pub const FLIGHT_TURNS: usize = 12;

/// The tools whose rules the flight prompt may describe conditionally.
pub const FLIGHT_TOOLS: &[&str] = &["flight_booking_links", "create_booking_link"];

pub const FLIGHT_PREAMBLE: &str = "\
You are Scout's flight desk. Another agent hands you a brief - a route, \
dates, passengers, sometimes an offer id - and you search, book or plan \
with the tools you have, then report back. You never speak to the \
traveller and you never buy anything.

Rules:
- Use search_flights for every fare question - never guess a price and \
never answer from memory. It asks the airlines directly, so its prices, \
times and flight numbers are current. Work out the airport codes yourself \
rather than asking: Amsterdam is AMS, Lisbon LIS, London LHR (or LON for \
all its airports), IATA codes, and dates go in as YYYY-MM-DD.
- When the brief says the dates are flexible, or asks whether another day \
is cheaper, pass flex_days (max 3) instead of searching each date \
yourself: it prices the whole window in one call. Do NOT pass flex_days \
otherwise - each day in the window is a separate paid search. Nobody can \
price a whole month; a week either side is the most that can be checked.
- A fare expires within minutes, so never reuse a price from an earlier \
brief; search again.
- When flight_booking_links is available, and the brief names an ignav \
offer_id to book, call it with that offer_id. It re-checks the fare and \
returns the airline's page and any resellers.
- When create_booking_link is available, and the brief asks to book a \
Duffel row, call it. Without it, nothing can be booked through Scout.
- When the brief is about more than one flight - a multi-city route, or a \
trip being assembled over several messages - build it with the trip tools \
rather than holding it in your head: add_trip_segment for each leg the \
moment you have a route and a date, add_trip_option for each flight \
found. A segment is one direction on one date, so a return is two \
segments. When a date or a leg changes, call update_trip_segment on that \
one segment. NEVER delete the trip and build it again, and never drop and \
re-add a segment to change it: both throw away every option parked on \
every other segment, and dropping renumbers everything after it. If the \
traveller is undecided between flights, park each with add_trip_option \
and decided=false; several options may sit on one segment. Finalising is \
the only thing that produces current prices and it costs a search per \
segment, so call finalise_trip when the brief says the trip is settled, \
not to check on it.
- Every trip tool hands back the whole trip. If a call failed, the trip \
did NOT change. When you make several edits, trust each call's own \
'changed' line over the snapshot beside it - a snapshot is from the \
moment that call ran - and believe the last snapshot, or call show_trip \
once at the end.
- Finish with one or two plain sentences: what you searched or changed, \
and any caveat - nothing flew that day, the window that was covered, a \
fare that moved. Give no prices, times or links in those sentences: the \
tool results travel back with your report and are read from there.";

/// The presentation rules that apply to what the specialist did, one
/// block per section, each included only when its trigger is present.
///
/// Returned with the findings rather than kept in the main prompt, so a
/// shopping turn never pays for a word of it. Written for the agent that
/// talks to the traveller.
pub fn guidance(findings: &[crate::specialist::Finding], markup_rate: f64) -> Vec<String> {
    // A failed finding is an error string, not a result; nothing below
    // applies to it.
    let ok = || findings.iter().filter(|f| !f.failed);
    let ran = |tool: &str| ok().any(|f| f.tool == tool);
    let searched = ran("search_flights");
    let windowed = ok().any(|f| {
        f.tool == "search_flights"
            && f.output.get("by_date").and_then(|v| v.as_array()).is_some_and(|a| !a.is_empty())
    });
    let planned = ok().any(|f| f.tool.contains("trip"));
    let mut out = Vec::new();
    if searched {
        out.push(SEARCH_GUIDANCE.to_string());
    }
    if windowed {
        out.push(WINDOW_GUIDANCE.to_string());
    }
    if ran("flight_booking_links") {
        out.push(IGNAV_LINKS_GUIDANCE.to_string());
    }
    if ran("create_booking_link") {
        out.push(DUFFEL_LINK_GUIDANCE.to_string());
    }
    if planned {
        out.push(TRIP_GUIDANCE.to_string());
    }
    if !out.is_empty() && markup_rate > 0.0 {
        out.push(format!(
            "Every flight price in these findings ALREADY includes a {} booking fee, which is \
             what the checkout charges. Quote the numbers unchanged and say once, in plain \
             words, that prices include that booking fee. Never add it on yourself.",
            crate::agent::percentage(markup_rate)
        ));
    }
    out
}

const SEARCH_GUIDANCE: &str = "\
Presenting search_flights findings: take every number verbatim - cheapest \
and fastest are ranked in Rust and are not yours to recompute - and quote \
the route field so the reply cannot drift onto a route nobody searched. \
Present the picks under their own headings - Cheapest, Fastest, Best balance \
- one to two options each, in that order, and offer every one you are \
given: they are already chosen from hundreds and no option appears under \
two headings, so anything you drop is a choice removed for no reason. When \
a group is empty, leave the heading out, and when one option is both \
cheapest and quickest say so. Every option carries price_status: 'bookable' \
is a live offer, quote it as a price; 'approximate' and 'unconfirmed' are \
fares seen elsewhere that still have to be checked on the seller's page - \
quote those as 'from EUR 180', say where they came from, and NEVER present \
one as a plain price or as 'the cheapest' without saying it is not bookable. \
When self_transfer is true the trip is two separate tickets: the traveller \
re-checks their bags and carries the risk if the first flight is late, so \
say that every time it appears. departing_at_local and arriving_at_local are \
each in the local time of their own airport, so NEVER subtract them to work \
out how long a flight takes; give journey length from the duration field. \
For any flight with a change of plane, name the connection airport and the \
wait from the connections list - 'changes at PVG, 3h 20m'; never work a \
layover out from flight numbers or your own knowledge. When changes_airport \
is true, say so loudly: the traveller lands at one airport and departs from \
another. A connection whose layover is null means the offer did not state \
the times - say that rather than guessing. Every leg carries an itinerary \
line already drawn for you - 'AMS 20:15 15.09 ✈ PVG 3h 20m ✈ HKG 20:35 \
16.09'. Put it on its own line under the option it belongs to, copied \
EXACTLY as given: do not retype, reorder, translate or rebuild it. A return \
trip has one such line per leg, outbound first. Say the price, the airline \
and what matters about the option, then the line. Prices are the whole trip \
for all passengers. Say what the notes say when they matter. found: 0 means \
nothing flies that route that day - say so plainly rather than guessing at \
alternatives. Flights have no product page to link; their count is set by \
the rows the search returned. Each option is its own block, separated by a \
blank line.";

const WINDOW_GUIDANCE: &str = "\
Presenting by_date: the search covered a window and by_date is the cheapest \
fare per day. Present it as a short list, cheapest day marked, and say which \
days were covered - the route field ends in ±N. A note may say some days \
could not be afforded; then say which window the answer actually covers.";

const IGNAV_LINKS_GUIDANCE: &str = "\
Presenting flight_booking_links: give the airline's link first and name any \
reseller that is cheaper, with both prices, rather than choosing for them. \
The links open with the flight already selected, so there is nothing to \
re-enter. The fare was re-checked at that moment: if a note says it has \
risen or fallen, tell them the new price before they open anything and \
never repeat the old one.";

const DUFFEL_LINK_GUIDANCE: &str = "\
Presenting create_booking_link: give the link on its own line and say \
plainly what it is - Duffel's own checkout, where they pick the flight and \
pay; Scout never sees passenger or card details. It CANNOT be pre-filled - \
it opens on its own search box - so repeat the route, date and price for \
them to enter, and say the link is single-use and short-lived. Never \
re-send an old one.";

const TRIP_GUIDANCE: &str = "\
Presenting a trip: the findings carry the whole trip as the tools returned \
it. When not_ready is present the trip cannot be priced and its reason \
says which segment is missing what; when it is absent the trip is \
complete. Say what it says, and never describe a trip from memory. Quote a \
trip's prices as of when each option was parked, never as a current total: \
only finalise_trip re-prices them. When finalise_trip ran, present both \
totals it returns and never drop the note about separate tickets: a link \
per segment is a ticket per segment, and the traveller carries the risk at \
every join. If the single-ticket total is missing, say that it is missing - \
it is not evidence that separate booking is better.";
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p scout-core flights::`
Expected: 8 pass.

- [ ] **Step 6: Commit**

```bash
git add crates/scout-core/src/flights.rs crates/scout-core/src/lib.rs crates/scout-core/src/agent.rs
git commit -m "feat(flights): the flight prompt, and guidance that travels with the findings

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 5: `FLIGHT_MODEL`

**Files:**
- Modify: `crates/scout-core/src/config.rs`
- Modify: `crates/scout-core/src/agent.rs` (`AgentDeps`)
- Modify: `crates/scout-core/src/core.rs:186-210`
- Modify: `.env.example`

- [ ] **Step 1: Write the failing test**

Add inside `mod tests` in `crates/scout-core/src/config.rs`:

```rust
    #[test]
    fn the_flight_agent_runs_on_the_main_model_unless_told_otherwise() {
        assert_eq!(load(&base_env()).unwrap().flight_model, crate::agent::MODEL);

        let mut env = base_env();
        env.insert("FLIGHT_MODEL", "some-other-model");
        assert_eq!(load(&env).unwrap().flight_model, "some-other-model");

        env.insert("FLIGHT_MODEL", " ");
        assert_eq!(load(&env).unwrap().flight_model, crate::agent::MODEL);
    }
```

- [ ] **Step 2: Run to watch it fail**

Run: `cargo test -p scout-core config::the_flight_agent`
Expected: compile error, no field `flight_model`.

- [ ] **Step 3: Add the field**

In `crates/scout-core/src/config.rs`, in `pub struct Config` after `minimax_base_url: String,`:

```rust
    /// The model the flight specialist runs on. Defaults to the main
    /// model; a separate name is how a cheaper or sharper model gets tried
    /// on flights alone.
    pub flight_model: String,
```

In `from_lookup`'s `Ok(Self { ... })` after the `minimax_base_url:` entry:

```rust
            flight_model: non_empty("FLIGHT_MODEL").unwrap_or_else(|| crate::agent::MODEL.to_string()),
```

In `crates/scout-core/src/agent.rs`, in `pub struct AgentDeps` after `pub llm: LlmClient,`:

```rust
    /// See `Config::flight_model`.
    pub flight_model: String,
```

In `crates/scout-core/src/core.rs`, in the `AgentDeps { ... }` literal after `llm,`:

```rust
            flight_model: cfg.flight_model.clone(),
```

In `.env.example` after the `#MINIMAX_BASE_URL=...` line:

```
# The model the flight agent runs on. Defaults to the main model.
#FLIGHT_MODEL=minimax-m3
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p scout-core config::`
Expected: pass. Also `cargo build -p scout-core` must succeed (the `AgentDeps` literal has one site).

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/config.rs crates/scout-core/src/agent.rs crates/scout-core/src/core.rs .env.example
git commit -m "feat(config): FLIGHT_MODEL, the model the flight agent runs on

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 6: Build the flight agent and offer it as `ask_flights`

**Files:**
- Modify: `crates/scout-core/src/flights.rs`
- Modify: `crates/scout-core/src/agent.rs` (`fare_market` stays `pub`; `markup_rate` becomes `pub(crate)`)

- [ ] **Step 1: Write the failing test**

Add inside `mod tests` in `crates/scout-core/src/flights.rs`:

```rust
    #[test]
    fn the_flight_desk_is_named_ask_flights_and_asks_for_a_brief() {
        // No provider is needed to build it: the tool is built from the
        // deps, and the model is only reached when it is called.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.duckdb");
        let core = crate::core::Core::start(crate::config::Config::for_test(path.to_str().unwrap()), None).unwrap();
        let run = scout_api::RunContext::telegram(1);
        let (events, _seen) = tokio::sync::mpsc::unbounded_channel();
        let budget = std::sync::Arc::new(crate::tools::budget::FlightBudget::default());
        let pulse = std::sync::Arc::new(crate::run::Pulse::default());

        let tool = ask_flights(&core.deps, &run, &[], budget, events, pulse);

        assert_eq!(rig::tool::Tool::name(&tool), "ask_flights");
        let d = rig::tool::Tool::description(&tool);
        for word in ["route", "dates", "passengers", "offer id"] {
            assert!(d.contains(word), "the parent must be told to put {word:?} in the brief: {d}");
        }
        let p = rig::tool::Tool::parameters(&tool);
        assert_eq!(p["required"], serde_json::json!(["brief"]));
    }
```

Check the exact constructor names before running: `grep -n "pub fn start\|pub fn telegram" crates/scout-core/src/core.rs crates/scout-api/src/lib.rs`. `Core::start(cfg, None)` is what `core.rs` tests use; `RunContext::telegram(chat_id)` exists in scout-api. If `core.deps` is not visible from `flights.rs` (it is `pub` on `Core` for the run loop), use whatever accessor `run.rs` uses.

- [ ] **Step 2: Run to watch it fail**

Run: `cargo test -p scout-core flights::the_flight_desk`
Expected: compile error, `ask_flights` not found.

- [ ] **Step 3: Write the builder and the wrapper**

In `crates/scout-core/src/agent.rs` change `fn markup_rate(d: &AgentDeps)` to `pub(crate) fn markup_rate(d: &AgentDeps)`.

Insert into `crates/scout-core/src/flights.rs` after `FLIGHT_PREAMBLE`:

```rust
use crate::agent::{fare_market, rules_for_available_tools, AgentDeps};
use crate::specialist::{Finding, Specialist, SPECIALIST_BUDGET};
use crate::tools::trips::{
    AddTripOptionTool, AddTripSegmentTool, ChooseTripOptionTool, DeleteTripTool,
    DropTripSegmentTool, FinaliseTripTool, ShowTripTool, UpdateTripSegmentTool,
};
use rig::client::CompletionClient;

/// The booking tools this install actually has, for the prompt filter.
fn available_tools(d: &AgentDeps) -> Vec<&'static str> {
    FLIGHT_TOOLS
        .iter()
        .copied()
        .filter(|tool| match *tool {
            "flight_booking_links" => d.ignav.is_some(),
            "create_booking_link" => d.duffel.is_some() && d.links_enabled && d.return_url.is_some(),
            _ => false,
        })
        .collect()
}

/// The nested agent: the flight prompt and every flight-shaped tool, wired
/// exactly as the main agent wired them before the split. Built per
/// request, like the main agent, because the tools capture the account.
pub fn build_flight_agent(
    d: &AgentDeps,
    run: &scout_api::RunContext,
    facts: &[(String, String)],
    budget: std::sync::Arc<crate::tools::budget::FlightBudget>,
) -> rig::agent::Agent<rig::providers::openai::completion::CompletionModel> {
    let (account_id, conversation_id) = (run.account_id, run.conversation_id);
    // Priced in the traveller's own currency, or Duffel's euros and
    // Ignav's dollars never get compared. Shared by search and finalise:
    // finalising re-prices through IgnavClient::search, which reads the
    // market; booking_links ignores it, since the id carries its own.
    let ignav = d.ignav.clone().map(|c| match fare_market(facts) {
        Some(market) => c.with_market(&market),
        None => c,
    });
    let mut builder = d
        .llm
        .agent(&d.flight_model)
        .preamble(&rules_for_available_tools(FLIGHT_PREAMBLE, &available_tools(d)))
        .tool(crate::tools::duffel::FlightSearchTool {
            duffel: d.duffel.clone(),
            store: d.store.clone(),
            account_id,
            budget: budget.clone(),
            shown: d.shown.clone(),
            conversation_id,
            ignav: ignav.clone(),
        })
        .tool(FinaliseTripTool {
            store: d.store.clone(),
            account_id,
            duffel: d.duffel.clone(),
            ignav,
            budget,
        })
        // Trip planning needs no provider, but a trip is a flight plan, so
        // it lives with the flights.
        .tool(AddTripSegmentTool { store: d.store.clone(), account_id })
        .tool(AddTripOptionTool {
            store: d.store.clone(),
            account_id,
            shown: d.shown.clone(),
            conversation_id,
        })
        .tool(ChooseTripOptionTool { store: d.store.clone(), account_id })
        .tool(ShowTripTool { store: d.store.clone(), account_id })
        .tool(UpdateTripSegmentTool { store: d.store.clone(), account_id })
        .tool(DropTripSegmentTool { store: d.store.clone(), account_id })
        .tool(DeleteTripTool { store: d.store.clone(), account_id });
    // Where an Ignav row can actually be bought.
    if let Some(ignav) = &d.ignav {
        builder = builder.tool(crate::tools::ignav::BookingLinksTool {
            client: ignav.clone(),
            shown: d.shown.clone(),
            conversation_id,
        });
    }
    // Duffel's hosted checkout: needs Duffel, Links enabled, and somewhere
    // to send people back to.
    if let (Some(duffel), Some(return_url)) =
        (&d.duffel, d.return_url.as_ref().filter(|_| d.links_enabled))
    {
        builder = builder.tool(crate::tools::duffel::BookingLinkTool {
            client: duffel.clone(),
            account_id,
            return_url: return_url.clone(),
        });
    }
    builder.default_max_turns(FLIGHT_TURNS).build()
}

/// The flight agent as the one tool the main agent sees.
pub fn ask_flights(
    d: &AgentDeps,
    run: &scout_api::RunContext,
    facts: &[(String, String)],
    budget: std::sync::Arc<crate::tools::budget::FlightBudget>,
    events: scout_api::EventSink,
    pulse: std::sync::Arc<crate::run::Pulse>,
) -> Specialist<rig::providers::openai::completion::CompletionModel> {
    let markup = crate::agent::markup_rate(d);
    Specialist {
        name: "ask_flights",
        description: "Scout's flight desk. Send it every question about flights, fares, \
                      airports, booking a flight, or a trip being planned. It sees nothing \
                      of the conversation, so the brief must be self-contained: route, \
                      dates, passengers, cabin, whether the dates are flexible, and any \
                      offer id the user is pointing at. It returns findings - the real \
                      search and booking results - plus guidance on presenting them."
            .to_string(),
        agent: build_flight_agent(d, run, facts, budget),
        events,
        pulse,
        guidance: Box::new(move |findings: &[Finding]| guidance(findings, markup)),
        budget: SPECIALIST_BUDGET,
    }
}
```

Move the `use` lines to the top of the file, above `FLIGHT_TURNS`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p scout-core flights::`
Expected: 9 pass. `FinaliseTripTool` field names come from `crates/scout-core/src/tools/trips.rs:513-531`; the literal above mirrors `build_agent`'s. If the compiler names a missing field, copy it from the old `build_agent` block (Task 7 removes that block; until then it is at `agent.rs:747-822`).

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/flights.rs crates/scout-core/src/agent.rs
git commit -m "feat(flights): build the flight agent and offer it as ask_flights

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 7: The main agent hands flights to the specialist

**Files:**
- Modify: `crates/scout-core/src/agent.rs` (`PREAMBLE`, `ALL_TOOLS`, `available_tools`, `preamble_with_profile`, `build_agent`, tests)
- Modify: `crates/scout-core/src/run.rs:68` (pass the sink)

- [ ] **Step 1: Update the tests to the new shape**

In `crates/scout-core/src/agent.rs` `mod tests`:

Replace the body of `the_preamble_never_describes_a_tool_this_agent_lacks` so that every `"search_flights"` becomes `"ask_flights"` (three occurrences). Replace `the_booking_fee_is_stated_in_the_preamble_when_one_is_charged` with:

```rust
    #[test]
    fn the_main_prompt_carries_no_flight_rules_and_no_fee() {
        // The point of the split. The fee now arrives with the findings
        // (flights::guidance) and in search_flights' own notes.
        let p = preamble_with_profile(&[], 0.03, ALL_TOOLS);
        assert!(!p.contains("Booking fee"), "got: {p}");
        for word in ["search_flights", "flex_days", "itinerary", "add_trip_segment", "price_status"] {
            assert!(!p.contains(word), "{word:?} belongs to the flight agent now");
        }
        assert!(p.contains("When ask_flights is available"), "got: {p}");
        assert!(p.contains("self-contained brief"), "got: {p}");
    }
```

Add:

```rust
    #[test]
    fn the_run_loop_hands_the_sink_to_the_agent_build() {
        // The specialist reports progress through the run's sink; without
        // it a flight question is a silent minute.
        let src = include_str!("run.rs");
        let src = &src[..src.find("#[cfg(test)]").expect("the tests must come last")];
        assert!(src.contains("build_agent(&core.deps, run, &facts, events.clone(), pulse.clone())"), "the sink and the pulse must reach build_agent");
    }
```

- [ ] **Step 2: Run to watch them fail**

Run: `cargo test -p scout-core agent::`
Expected: `the_main_prompt_carries_no_flight_rules_and_no_fee` and `the_run_loop_hands_the_sink_to_the_agent_build` FAIL; the renamed one fails on `ask_flights`.

- [ ] **Step 3: Cut the preamble**

In `PREAMBLE` in `crates/scout-core/src/agent.rs`:

1. Delete the whole rule that begins `- When search_flights is available, use it for every flight question` through `give the numbers and let the user buy from the airline.` (lines 75-148 before this task).
2. Delete the whole rule that begins `- When someone is planning more than one flight` through `it is not evidence that separate booking is better.` (149-182).
3. Delete the whole rule that begins `- If a booking fee is listed below` through `price without it.` (183-187).
4. In the rule `- Always include the price (with currency) and a direct link for every option you present.`, replace `At most 5 options, best first - this is about products; flights have no product page to link and their count is set by the rows the flight search returns. If you genuinely` with `At most 5 options, best first. If you genuinely`.
5. Where rule 1 was, insert:

```
- When ask_flights is available, send it every question about flights, \
fares, airport codes, booking a flight, or a trip being planned - never a \
web search, and never compare_prices, whose per-unit arithmetic means \
nothing for a flight. It sees nothing of this conversation, so write a \
self-contained brief: route, dates, passengers, cabin, whether the dates \
are flexible, and any offer id the user is pointing at. If the user has \
not given something a search needs, ask them before calling it. Its \
result carries a summary, findings and guidance: present the findings \
following that guidance, take every number, time and link from the \
findings verbatim - never from the summary and never from memory - and \
relay the summary's caveats in plain words (what could not be searched, \
what is missing, that Scout cannot book). A fare expires within minutes: \
for a later question, ask ask_flights again rather than repeating one.
```

Also fix the opening line so the prompt no longer claims to be only a product assistant: `You are Scout, a product-research assistant living in a Telegram chat.` → `You are Scout, a product and travel research assistant living in a chat.`

- [ ] **Step 4: Rewire `build_agent` and the helpers**

In `crates/scout-core/src/agent.rs`:

`ALL_TOOLS`:
```rust
pub const ALL_TOOLS: &[&str] = &["search_bol", "ask_flights"];
```

`available_tools` arm:
```rust
            "ask_flights" => d.duffel.is_some() || d.ignav.is_some(),
```

In `preamble_with_profile`, delete the `if markup_rate > 0.0 { ... }` block and its comment. Keep the `markup_rate` parameter (callers pass it) but rename it `_markup_rate`; the fee now travels in the guidance.

Remove the `use crate::tools::trips::{...}` import at the top of the file.

`build_agent` signature and body:

```rust
pub fn build_agent(
    d: &AgentDeps,
    run: &scout_api::RunContext,
    facts: &[(String, String)],
    events: scout_api::EventSink,
    pulse: std::sync::Arc<crate::run::Pulse>,
) -> rig::agent::Agent<openai::completion::CompletionModel> {
```

Delete the seven `.tool(AddTripSegmentTool ...)` through `.tool(DeleteTripTool ...)` lines and their comment. Replace everything from `// Either provider can answer a flight question` down to (and including) the `BookingLinkTool` block with:

```rust
    // The flight desk, offered whenever at least one provider can answer
    // a flight question. Every flight-shaped tool lives inside it; the
    // main agent sees one tool and a report.
    if d.duffel.is_some() || d.ignav.is_some() {
        builder = builder.tool(crate::flights::ask_flights(d, run, facts, flights, events, pulse));
    }
```

The `flights` binding (the `FlightBudget`) stays where it is created at the top of `build_agent`.

In `crates/scout-core/src/run.rs`: `let agent = build_agent(&core.deps, run, &facts, events.clone(), pulse.clone());` (the `pulse` binding was created just above it in Task 3b).

Note from the Task 3b review: the pulse is touched per stream item, and a
nested stream is silent during a nested tool call, so one nested tool call
longer than `STREAM_STALL` (90 s) still trips the outer guard. That is the
pre-existing invariant (`STREAM_STALL`'s comment: every tool carries its own
timeout well under it) and `search_flights` runs its window in parallel, so
no change is needed; do not raise `SPECIALIST_BUDGET` above what a single
tool call can take without also touching the pulse from inside the tool.

- [ ] **Step 5: Build and run the whole crate's tests**

Run: `cargo test -p scout-core`
Expected: all pass. Unused-import warnings for `FlightSearchTool`/`percentage` in `agent.rs` mean a stale import; remove it. If `every_conditional_rule_is_wired_up` fails, a rule opening with "When ... is available," still names a moved tool: re-check step 3.

- [ ] **Step 6: Run the workspace**

Run: `cargo test --workspace`
Expected: all pass. `scout-web` and `scout-telegram` do not call `build_agent` directly, so nothing else changes.

- [ ] **Step 7: Commit**

```bash
git add crates/scout-core/src/agent.rs crates/scout-core/src/run.rs
git commit -m "feat(agent): the main agent hands every flight question to ask_flights

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 8: Docs

**Files:**
- Modify: `README.md`
- Modify: `docs/BOARD.md`

- [ ] **Step 1: README**

In the `## How it works` diagram, replace the `23 tools` heading and the flight rows so the diagram reads:

```
Telegram ──► bot.rs ─────┐
Browser  ──► scout-web ──┴► core ──► rig agent ──────► 14 tools + the flight desk
                │                                    │
                │  streams progress + answer         ├─ search_web        Kagi + Perplexity, merged
                │  back into one edited message      ├─ search_secondhand eBay / Marktplaats / Vinted
                │                                    ├─ search_bol        bol.com catalogue *
                ▼                                    ├─ fetch_page        + headless-Chrome fallback
        link verification                            ├─ compare_prices    deterministic, in Rust
        (nothing dead ships)                         ├─ query_purchases   ─┐
                                                     ├─ record_purchase    ├─ DuckDB
                                                     ├─ remember_fact      │
                                                     ├─ forget_fact       ─┘
                                                     ├─ reminders (create/list/cancel)
                                                     └─ ask_flights *  ──► flight agent ──► 10 tools
                                                                             ├─ search_flights    Duffel + Ignav, merged
                                                                             ├─ flight_booking_links  airline pages, pre-filled
                                                                             ├─ create_booking_link   Duffel hosted checkout
                                                                             ├─ add / update / drop segment,
                                                                             │  add / choose option, show, delete
                                                                             │  a named multi-city plan
                                                                             └─ finalise_trip     re-prices it all
```

Below the `*` note, add a paragraph:

```
The flight agent is a second rig agent with its own prompt, called by the
first as one tool. It sees only the brief the main agent writes, and it
answers with findings — the real tool outputs — plus the rules for
presenting them, generated in Rust from what it actually did. A shopping
question never carries a word about flights; a flight question carries only
the rules its findings need. The same wrapper (`specialist.rs`) is how a
trip builder, a hotel agent and an experience agent will be added.
```

In the configuration table, after the `MINIMAX_BASE_URL` row:

```
| `FLIGHT_MODEL` | no | the main model | the model the flight agent runs on |
```

Update the test count in `cargo test runs **N tests**` to the number `cargo test --workspace 2>&1 | grep -E "^test result" | awk '{s+=$4} END {print s}'` prints.

- [ ] **Step 2: Board**

In `docs/BOARD.md`, add under `## In progress` (replacing `_(nothing)_`):

```
- [ ] **Flight agent as a tool** — `ask_flights`: a specialist rig agent with the flight prompt and every flight and trip tool, called by the main agent with a brief; findings plus Rust-generated guidance come back. Spec: `docs/superpowers/specs/2026-09-08-flight-specialist-design.md`
```

(The line moves to Done with the merge hash when the branch merges.)

- [ ] **Step 3: Commit**

```bash
git add README.md docs/BOARD.md
git commit -m "docs: the flight agent, in the README and on the board

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 9: Finish

- [ ] **Step 1: Full test run and clippy**

```bash
cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings
```

Expected: green. Fix anything clippy names before merging.

- [ ] **Step 2: Merge, push, deploy** (the user's standing workflow)

```bash
git checkout main && git merge --no-ff feat/flight-specialist -m "Merge: the flight agent is a tool the main agent calls

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>" && git push
```

Then move the board line to Done with the merge hash and the date, commit, and run `scripts/deploy-k3s.sh`. Verify with `ssh $SCOUT_SSH "kubectl -n scout logs deploy/scout --tail=30"` for `scout is up`.

- [ ] **Step 3: Live check**

On the deployed bot, ask for a return flight with flexible dates and watch for the progress lines `✈️ asking the flight desk: …` and `✈️ searching …`. Then say "book the second one" and confirm the reply carries booking links.
