/// How a piece of streamed text changed.
///
/// `Replace` is not an optimisation escape hatch — it is required. The
/// answer can *shrink*: `strip_thinking` discards everything before a
/// closing tag that has no opener, because such a closer means the text
/// began inside a thinking block. A client that only ever appends would go
/// on showing reasoning the run has already retracted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TextUpdate {
    Append(String),
    Replace(String),
}

impl TextUpdate {
    /// Moves accumulated text forward by this update.
    ///
    /// The counterpart of `Shown::update`. Every client does exactly this,
    /// which is why it lives here rather than being written twice.
    pub fn apply(&self, into: &mut String) {
        match self {
            TextUpdate::Append(delta) => into.push_str(delta),
            TextUpdate::Replace(text) => {
                into.clear();
                into.push_str(text);
            }
        }
    }
}

/// What a client has been shown so far, and what to send it next.
///
/// Producers hold one of these per stream of text and feed it the whole
/// text each time; it works out the smallest honest update. `None` means
/// nothing changed and no event is worth sending.
#[derive(Debug, Default, Clone)]
pub struct Shown(String);

impl Shown {
    pub fn update(&mut self, next: &str) -> Option<TextUpdate> {
        if next == self.0 {
            return None;
        }
        let update = match next.strip_prefix(self.0.as_str()) {
            Some(rest) => TextUpdate::Append(rest.to_string()),
            None => TextUpdate::Replace(next.to_string()),
        };
        self.0 = next.to_string();
        Some(update)
    }
}

/// What the agent has to say while it works, independent of who is
/// listening.
///
/// `Answer` and `Thinking` carry a `TextUpdate` rather than the whole text —
/// see `TextUpdate` for why that update is sometimes a `Replace` rather than
/// an append. `Tool` and `Notice` stay whole text: each is one discrete
/// sentence that never grows, so there is nothing for an update to be
/// relative to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AgentEvent {
    /// A tool started, already rendered as a human sentence.
    Tool(String),
    /// The answer, as it changes.
    Answer(TextUpdate),
    /// Reasoning, as it changes. Shown only while the answer is empty.
    Thinking(TextUpdate),
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
    /// Where a reminder made during this run should be delivered, or
    /// `None` when there is nowhere — a browser is not a delivery channel.
    /// A run with `None` is not offered the reminder tool at all, so the
    /// model never promises something that would silently never arrive.
    pub reply_to: Option<ReplyTo>,
}

impl ReplyTo {
    /// A Telegram chat, by id. The channel string is written once here so
    /// no caller has to spell it.
    pub fn telegram(chat_id: i64) -> Self {
        Self { channel: "telegram".to_string(), address: chat_id.to_string() }
    }
}

/// Who said a thing, as a reader sees it.
///
/// An enum rather than a string so a client cannot invent a third role, and
/// named for what appears on screen rather than for `rig`'s vocabulary — a
/// page that has to translate "assistant" into "Scout" is a page that will
/// eventually translate it inconsistently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Role {
    You,
    Scout,
}

/// One thing said in a conversation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Turn {
    pub role: Role,
    pub text: String,
}

/// One thread in the browser's list. `current` is the one a Telegram
/// message would continue and the mirror follows; exactly one row has it
/// whenever the list is non-empty.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Thread {
    pub id: i64,
    /// Null until the first answer lands.
    pub title: Option<String>,
    pub pinned: bool,
    /// RFC 3339, UTC. A string so the page does not need a date library
    /// to show "2h ago".
    pub updated_at: String,
    pub current: bool,
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
        emit(&tx, AgentEvent::Answer(TextUpdate::Append("still working".to_string())));
    }

    #[test]
    fn events_arrive_in_the_order_they_were_sent() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        emit(&tx, AgentEvent::Tool("searching Kagi".to_string()));
        emit(&tx, AgentEvent::Thinking(TextUpdate::Append("comparing".to_string())));
        emit(&tx, AgentEvent::Answer(TextUpdate::Append("The cheapest".to_string())));
        drop(tx);

        let mut got = Vec::new();
        while let Ok(e) = rx.try_recv() {
            got.push(e);
        }
        assert_eq!(
            got,
            vec![
                AgentEvent::Tool("searching Kagi".to_string()),
                AgentEvent::Thinking(TextUpdate::Append("comparing".to_string())),
                AgentEvent::Answer(TextUpdate::Append("The cheapest".to_string())),
            ]
        );
    }

    #[test]
    fn an_event_survives_the_json_round_trip_both_sides_will_use() {
        // In 2b-2b this crosses a socket. The point of one shared crate is
        // that the two ends cannot disagree about what an event is.
        let each_kind = vec![
            AgentEvent::Tool("🔎 searching: wasmiddel".to_string()),
            AgentEvent::Answer(TextUpdate::Append("The cheapest is".to_string())),
            AgentEvent::Thinking(TextUpdate::Append("comparing fares".to_string())),
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

    #[test]
    fn growing_text_produces_appends_and_shrinking_text_produces_a_replace() {
        let mut shown = Shown::default();
        assert_eq!(shown.update("Here"), Some(TextUpdate::Append("Here".into())));
        assert_eq!(shown.update("Here are"), Some(TextUpdate::Append(" are".into())));
        // Not an extension, so the client has to be told to start over.
        assert_eq!(shown.update("Hello"), Some(TextUpdate::Replace("Hello".into())));
    }

    #[test]
    fn text_that_did_not_change_produces_no_event_at_all() {
        // Otherwise every streamed token inside a <think> block would send
        // an empty Append.
        let mut shown = Shown::default();
        shown.update("same");
        assert_eq!(shown.update("same"), None);
    }

    #[test]
    fn becoming_empty_is_a_replace_and_not_silence() {
        // The retraction. `strip_thinking` discards everything before a
        // stray closer, so the answer can go from text to nothing, and a
        // client that is not told will keep showing reasoning.
        let mut shown = Shown::default();
        shown.update("secret reasoning here");
        assert_eq!(shown.update(""), Some(TextUpdate::Replace(String::new())));
    }

    #[test]
    fn applying_updates_in_order_reproduces_the_text_that_produced_them() {
        let mut shown = Shown::default();
        let mut client = String::new();
        for step in ["a", "ab", "abc", "xyz", "", "done"] {
            if let Some(update) = shown.update(step) {
                update.apply(&mut client);
            }
            assert_eq!(client, step, "client drifted from the source text");
        }
    }

    #[test]
    fn a_thread_serialises_with_the_names_the_page_reads() {
        let t = Thread {
            id: 7,
            title: None,
            pinned: true,
            updated_at: "2026-09-05T10:00:00Z".to_string(),
            current: false,
        };
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json["id"], 7);
        assert!(json["title"].is_null());
        assert_eq!(json["pinned"], true);
        assert_eq!(json["updated_at"], "2026-09-05T10:00:00Z");
        assert_eq!(json["current"], false);
    }
}
