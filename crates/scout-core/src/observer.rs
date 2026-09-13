//! What a run hands to everything that works for it: where to report
//! progress, proof of life for the stall guard, and the trace.
//!
//! One struct rather than three parameters because the flight desk and its
//! specialist need all three, and a trace recorded by two different pieces
//! of code would disagree about what a failed tool looks like.

use scout_api::{AgentEvent, EventSink, TraceFrame, TraceRow};
use std::collections::HashMap;
use std::sync::Mutex;

pub struct Observer {
    pub events: EventSink,
    pub pulse: crate::run::Pulse,
    pub run_id: i64,
    rows: Mutex<Vec<TraceRow>>,
    /// Calls started and not yet finished, by rig's internal call id, to
    /// the row they opened and the instant they started.
    open: Mutex<HashMap<String, (usize, std::time::Instant)>>,
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl Observer {
    pub fn new(events: EventSink, run_id: i64) -> Self {
        Self {
            events,
            pulse: crate::run::Pulse::default(),
            run_id,
            rows: Mutex::new(Vec::new()),
            open: Mutex::new(HashMap::new()),
        }
    }

    fn rows(&self) -> std::sync::MutexGuard<'_, Vec<TraceRow>> {
        self.rows.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self, frame: TraceFrame) {
        scout_api::emit(&self.events, AgentEvent::Trace(frame));
    }

    /// Sent once, first, so the page knows which run a live panel belongs
    /// to before any row arrives.
    pub fn announce(&self) {
        self.emit(TraceFrame::Run { run_id: self.run_id });
    }

    pub fn tool_started(&self, call_id: &str, tool: &str, args: serde_json::Value, nested: bool) {
        let seq = {
            let mut rows = self.rows();
            let seq = rows.len() as i64;
            rows.push(TraceRow {
                seq, kind: "tool".into(), tool: Some(tool.to_string()), args: Some(args.clone()),
                nested, started_at: now_iso(), duration_ms: None, status: None, detail: None,
                result: None, truncated: false,
            });
            seq
        };
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(call_id.to_string(), (seq as usize, std::time::Instant::now()));
        self.emit(TraceFrame::Started { seq, tool: tool.to_string(), args, nested });
    }

    /// The tool answered. rig hands a tool's `Err` to the model as the
    /// error's text, and every Scout tool returns a serialised struct, so a
    /// result that is not JSON is a tool that failed — the rule the flight
    /// desk's collector already applies.
    pub fn tool_finished(&self, call_id: &str, text: &str) {
        let Some((index, started)) = self.open.lock().unwrap_or_else(|e| e.into_inner()).remove(call_id) else {
            return;
        };
        let failed = serde_json::from_str::<serde_json::Value>(text).is_err();
        let duration_ms = started.elapsed().as_millis() as i64;
        let status = if failed { "failed" } else { "ok" }.to_string();
        let detail = failed.then(|| text.to_string());
        let seq = {
            let mut rows = self.rows();
            let row = &mut rows[index];
            row.duration_ms = Some(duration_ms);
            row.status = Some(status.clone());
            row.detail = detail.clone();
            row.result = Some(text.to_string());
            row.seq
        };
        self.emit(TraceFrame::Finished { seq, duration_ms, status, detail });
    }

    /// Something the run did on its own: a repair, a salvage, a failure.
    /// Worded exactly as the log line beside it.
    pub fn event(&self, detail: &str, error: bool) {
        let seq = {
            let mut rows = self.rows();
            let seq = rows.len() as i64;
            rows.push(TraceRow {
                seq, kind: "event".into(), tool: None, args: None, nested: false,
                started_at: now_iso(), duration_ms: None,
                status: Some(if error { "failed" } else { "ok" }.to_string()),
                detail: Some(detail.to_string()), result: None, truncated: false,
            });
            seq
        };
        self.emit(TraceFrame::Event { seq, detail: detail.to_string(), error });
    }

    /// Everything recorded so far, for saving. Leaves the observer empty.
    pub fn take_rows(&self) -> Vec<TraceRow> {
        std::mem::take(&mut *self.rows())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scout_api::{AgentEvent, TraceFrame};
    use serde_json::json;

    fn observer() -> (Observer, tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) {
        let (events, seen) = tokio::sync::mpsc::unbounded_channel();
        (Observer::new(events, 42), seen)
    }

    fn frames(seen: &mut tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) -> Vec<TraceFrame> {
        let mut out = Vec::new();
        while let Ok(e) = seen.try_recv() {
            if let AgentEvent::Trace(f) = e { out.push(f) }
        }
        out
    }

    #[test]
    fn a_tool_that_starts_and_finishes_is_one_row_with_a_duration() {
        let (o, mut seen) = observer();
        o.tool_started("c1", "search_web", json!({"query": "beans"}), false);
        o.tool_finished("c1", r#"{"hits": 3}"#);
        let rows = o.take_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tool.as_deref(), Some("search_web"));
        assert_eq!(rows[0].status.as_deref(), Some("ok"));
        assert!(rows[0].duration_ms.is_some());
        assert_eq!(rows[0].result.as_deref(), Some(r#"{"hits": 3}"#));
        assert!(!rows[0].nested);
        let f = frames(&mut seen);
        assert!(matches!(f[0], TraceFrame::Started { seq: 0, ref tool, .. } if tool == "search_web"));
        assert!(matches!(f[1], TraceFrame::Finished { seq: 0, ref status, .. } if status == "ok"));
    }

    #[test]
    fn a_result_that_is_not_json_is_a_failed_tool() {
        // rig hands a tool's Err to the model as its text, and every Scout
        // tool returns a struct — so text that is not JSON is the error.
        let (o, mut seen) = observer();
        o.tool_started("c1", "search_flights", json!({}), true);
        o.tool_finished("c1", "duffel api error (status 429): slow down");
        let rows = o.take_rows();
        assert_eq!(rows[0].status.as_deref(), Some("failed"));
        assert_eq!(rows[0].detail.as_deref(), Some("duffel api error (status 429): slow down"));
        assert!(rows[0].nested);
        assert!(matches!(frames(&mut seen)[1], TraceFrame::Finished { ref detail, .. } if detail.is_some()));
    }

    #[test]
    fn a_finish_for_a_call_never_started_is_ignored() {
        let (o, _seen) = observer();
        o.tool_finished("ghost", "{}");
        assert!(o.take_rows().is_empty());
    }

    #[test]
    fn an_event_is_a_row_of_its_own() {
        let (o, mut seen) = observer();
        o.tool_started("c1", "search_web", json!({}), false);
        o.event("dead links in reply; asking the agent to correct it", false);
        o.event("the model call failed", true);
        let rows = o.take_rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].kind, "event");
        assert_eq!(rows[1].seq, 1);
        assert_eq!(rows[2].detail.as_deref(), Some("the model call failed"));
        assert_eq!(rows[2].status.as_deref(), Some("failed"), "an error event says so");
        let f = frames(&mut seen);
        assert!(matches!(f[2], TraceFrame::Event { error: true, .. }));
    }

    #[test]
    fn the_run_frame_goes_out_first() {
        let (o, mut seen) = observer();
        o.announce();
        assert!(matches!(frames(&mut seen)[0], TraceFrame::Run { run_id: 42 }));
    }
}
