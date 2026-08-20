/// What the agent has to say while it works, independent of who is
/// listening.
///
/// Every variant carries the whole text rather than a delta, because that is
/// what `Live::show` already takes and `Live` diffs it against what is on
/// screen. A socket would rather have deltas; that is a phase-2b question,
/// and changing it here would alter behaviour while claiming not to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    /// A tool started, already rendered as a human sentence.
    Tool(String),
    /// The answer so far, with reasoning stripped out.
    Answer(String),
    /// Reasoning so far. Shown only while the answer is still empty.
    Thinking(String),
    /// A line from the run itself rather than from the model — today only
    /// the wrap-up notice when a run is salvaged.
    Notice(String),
}

/// The sending half. `run_agent` takes one by value so that returning drops
/// it, which closes the channel and ends the renderer.
pub type EventSink = tokio::sync::mpsc::UnboundedSender<AgentEvent>;

/// Hands an event to whoever is listening, if anyone is.
///
/// A send fails only when the receiver has gone, which means nobody is
/// watching this run any more. That is not a reason to abandon work the
/// user may still be charged for, so the error is dropped on purpose.
pub fn emit(sink: &EventSink, event: AgentEvent) {
    let _ = sink.send(event);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emitting_into_a_closed_channel_is_not_an_error() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        drop(rx);
        // Nobody is listening. A run that is still doing useful work must
        // not be brought down because the chat went away.
        emit(&tx, AgentEvent::Answer("still working".to_string()));
    }

    #[test]
    fn events_arrive_in_the_order_they_were_sent() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        emit(&tx, AgentEvent::Tool("searching Kagi".to_string()));
        emit(&tx, AgentEvent::Thinking("comparing".to_string()));
        emit(&tx, AgentEvent::Answer("The cheapest".to_string()));
        drop(tx);

        let mut got = Vec::new();
        while let Ok(e) = rx.try_recv() {
            got.push(e);
        }
        assert_eq!(
            got,
            vec![
                AgentEvent::Tool("searching Kagi".to_string()),
                AgentEvent::Thinking("comparing".to_string()),
                AgentEvent::Answer("The cheapest".to_string()),
            ]
        );
    }
}
