/// What the agent has to say while it works, independent of who is
/// listening.
///
/// Every variant carries the whole text rather than a delta, because that is
/// what `Live::show` already takes and `Live` diffs it against what is on
/// screen. A socket would rather have deltas; that is a phase-2b question,
/// and changing it here would alter behaviour while claiming not to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

/// One thing to deliver, to one address, on one channel.
///
/// `text` is written here so that a browser and a chat say the same
/// sentence. `channel` is what the caller asked for and is echoed back
/// rather than needed — the Telegram adapter never reads it — because on a
/// wire a row that names its own channel can be logged, batched or
/// forwarded without carrying the query along beside it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DueDelivery {
    pub id: i64,
    pub channel: String,
    pub address: String,
    pub text: String,
}

/// Where the side effects of a run should be delivered.
///
/// A run produces more than an answer: a reminder created mid-conversation
/// has to be sent somewhere later. That destination is a property of *where
/// the request arrived*, not of who made it — in a group chat the address is
/// the group, so a reminder asked for there goes back there. Resolving it
/// from the account's `deliveries` row instead would send it wherever that
/// account last spoke: `note_chat` records incoming chats, group or private
/// alike, last write wins. The destination would then depend on unrelated
/// later activity — quietly, and long after the reminder was made.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReplyTo {
    pub channel: String,
    pub address: String,
}

/// Who a run is for, which thread it belongs to, and where its side effects
/// should go.
///
/// Grouped rather than passed as three parameters because two of them are
/// bare `i64` and sat next to each other. `build_agent(d, account_id,
/// conversation_id, ..)` accepted them transposed, and a mutation check
/// confirmed nothing caught it: the reminder and flight tools were simply
/// wired to the wrong numbers, silently.
///
/// This does not make the mistake impossible — `account_id: conversation_id`
/// still compiles, and that was checked rather than assumed. What it does is
/// move the error from a position to a name, so making it now means writing
/// a line that is wrong on its face. That is worth more here than a test
/// would be, because observing the wiring from outside would mean reaching
/// inside a built agent to see which numbers its tools captured.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunContext {
    pub account_id: i64,
    pub conversation_id: i64,
    pub reply_to: ReplyTo,
}

impl ReplyTo {
    /// A Telegram chat, by id. The channel string is written once here so
    /// no caller has to spell it.
    pub fn telegram(chat_id: i64) -> Self {
        Self { channel: "telegram".to_string(), address: chat_id.to_string() }
    }
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

    #[test]
    fn an_event_survives_the_json_round_trip_both_sides_will_use() {
        // In 2b-2b this crosses a socket. The point of one shared crate is
        // that the two ends cannot disagree about what an event is.
        let each_kind = vec![
            AgentEvent::Tool("🔎 searching: wasmiddel".to_string()),
            AgentEvent::Answer("The cheapest is".to_string()),
            AgentEvent::Thinking("comparing fares".to_string()),
            AgentEvent::Notice("wrapped up early".to_string()),
        ];
        for event in each_kind {
            let wire = serde_json::to_string(&event).unwrap();
            let back: AgentEvent = serde_json::from_str(&wire).unwrap();
            assert_eq!(back, event, "round trip changed the event: {wire}");
        }
    }

    #[test]
    fn a_reply_to_survives_a_round_trip_and_names_its_channel() {
        // W4 puts this on a wire, so it has to serialise; the Telegram
        // constructor exists so an adapter cannot spell "telegram" wrong.
        let r = ReplyTo::telegram(-100123);
        assert_eq!(r.channel, "telegram");
        assert_eq!(r.address, "-100123");

        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<ReplyTo>(&json).unwrap(), r);
    }
}
