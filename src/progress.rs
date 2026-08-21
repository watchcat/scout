//! Live feedback while the agent works.
//!
//! A product search runs a dozen tool calls before it writes a word, so the
//! chat sits silent for the better part of a minute. One message is sent up
//! front and edited as work happens — first with what the agent is doing,
//! then with its answer as it streams in.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use teloxide::prelude::*;
use teloxide::types::{MessageId, ParseMode};

/// Slowest a single chat's message is rewritten.
///
/// Was 1200ms, on the reasoning that Telegram allows about a message a
/// second to one chat and 0.83 is under it. That held for short answers and
/// failed for long ones: a research run that went several minutes, editing
/// the same message ~50 times a minute throughout, drew `RetryAfter(238s)`
/// — a four-minute ban that cost the traveller the answer.
///
/// So it is paced to teloxide's own `messages_per_min_chat` default of 20,
/// with a little margin. A frame every three seconds still reads as alive,
/// which is all this is for; the per-second figure was never the binding
/// constraint on a run that lasts minutes.
const MIN_EDIT_INTERVAL: Duration = Duration::from_millis(3200);

/// What Telegram allows one bot across *all* of its chats, not per chat.
/// teloxide's own throttle adaptor carries the same figure as its default.
const TELEGRAM_EDITS_PER_SECOND: usize = 30;

/// Slice of that budget each reply in flight is entitled to.
///
/// Derived from the ceiling with a couple of milliseconds of margin, so the
/// arithmetic lands under it rather than exactly on it — at a hundred
/// replies an exact share divides out to 30.0003 edits a second, and being
/// right on the line is how you find out where the line really is.
const PER_STREAM: Duration = Duration::from_millis(1000 / TELEGRAM_EDITS_PER_SECOND as u64 + 2);

/// How long to wait between frames, given how many replies are streaming.
///
/// [`MIN_EDIT_INTERVAL`] alone is a per-chat rule, and Telegram's harder
/// limit is per bot: at 1.2s a frame it takes only thirty-six simultaneous
/// replies to saturate the whole token, and the rest come back 429. This
/// starts widening at thirty-five and changes nothing below that; above it
/// every reply slows down together, which is the cheapest fair way to stay
/// inside one shared budget.
///
/// Note that teloxide's `throttle` adaptor would not cover this. It forwards
/// `edit_message_text` to the inner requester unthrottled, and edits are
/// nearly all of what a streaming reply sends.
fn edit_interval(active: usize) -> Duration {
    let active = u32::try_from(active).unwrap_or(u32::MAX);
    MIN_EDIT_INTERVAL.max(PER_STREAM.saturating_mul(active))
}

/// Counts one reply for as long as it is in flight.
///
/// A guard rather than a pair of calls in the handler: a request can end by
/// early return, by `?`, or by panic, and every one of those has to give the
/// slot back. A count that only ever rises would throttle the bot to a
/// standstill and look like Telegram's fault.
struct StreamSlot(Arc<AtomicUsize>);

impl StreamSlot {
    fn take(streams: Arc<AtomicUsize>) -> Self {
        streams.fetch_add(1, Ordering::Relaxed);
        Self(streams)
    }

    fn active(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// How much of the model's reasoning to keep on screen. It runs to
/// paragraphs; the newest sentence or two is the part worth reading.
const THINKING_TAIL: usize = 220;

/// The last `max` characters, cut at a word boundary and marked with a
/// leading ellipsis when anything was dropped.
fn tail(text: &str, max: usize) -> String {
    let text = text.trim();
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let cut: String = text.chars().skip(count - max).collect();
    let cut = cut.split_once(' ').map(|(_, rest)| rest).unwrap_or(&cut);
    format!("…{}", cut.trim_start())
}

/// Telegram's HTML parse mode needs these three escaped; anything else in a
/// reasoning trace is literal.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// One Telegram message, repeatedly rewritten.
pub struct Live {
    bot: Bot,
    chat_id: ChatId,
    message: Option<MessageId>,
    last_edit: Option<Instant>,
    shown: String,
    /// When Telegram's flood control said to leave this chat alone until.
    ///
    /// Measured: a long run drew `RetryAfter(238s)` on one chat. Editing
    /// through a penalty only deepens it, so ordinary progress frames stop
    /// until it lifts. A forced frame — the answer itself — still tries,
    /// and its caller falls back to a send that waits properly.
    quiet_until: Option<Instant>,
    /// Held for the lifetime of the reply; see [`StreamSlot`].
    slot: StreamSlot,
}

impl Live {
    /// `streams` is the process-wide count of replies in flight, shared by
    /// every `Live`. Telegram's ceiling is per bot token, so pacing can only
    /// be decided from what the whole process is doing.
    pub fn new(bot: Bot, chat_id: ChatId, streams: Arc<AtomicUsize>) -> Self {
        Self {
            bot,
            chat_id,
            message: None,
            last_edit: None,
            shown: String::new(),
            quiet_until: None,
            slot: StreamSlot::take(streams),
        }
    }

    /// The message being edited, once it exists — needed to remember what was
    /// said so a later 👍 can be resolved to it.
    pub fn message_id(&self) -> Option<MessageId> {
        self.message
    }

    /// Text currently displayed.
    pub fn shown(&self) -> &str {
        &self.shown
    }

    /// Rewrites the message. Skipped when nothing changed or when the last
    /// edit was too recent, unless `force` (the final answer must always
    /// land). Failures are logged, never fatal: losing a progress frame must
    /// not lose the answer.
    /// True when the message now shows `text`.
    pub async fn show(&mut self, text: &str, force: bool) -> bool {
        self.render(text, force, None).await
    }

    /// The model's reasoning, in italics — it is worth watching while the
    /// search runs, but it must never read as part of the answer.
    pub async fn show_thinking(&mut self, text: &str) -> bool {
        let text = tail(text, THINKING_TAIL);
        self.render(&text, false, Some(format!("<i>💭 {}</i>", escape_html(&text)))).await
    }

    async fn render(&mut self, text: &str, force: bool, html: Option<String>) -> bool {
        let text = text.trim();
        if text.is_empty() {
            return false;
        }
        if text == self.shown {
            return true;
        }
        if !force && !self.due(Instant::now(), self.slot.active()) {
            return false;
        }
        // Long answers are chunked by the caller; a frame that overflows is
        // clipped rather than dropped.
        let text: String = text.chars().take(crate::text::TELEGRAM_LIMIT).collect();
        let body = html.clone().unwrap_or_else(|| text.clone());
        let parse_mode = html.is_some().then_some(ParseMode::Html);
        let sent = match self.message {
            Some(id) => {
                let mut req = self.bot.edit_message_text(self.chat_id, id, body);
                req.parse_mode = parse_mode;
                req.await.map(|m| m.id)
            }
            None => {
                let mut req = self.bot.send_message(self.chat_id, body);
                req.parse_mode = parse_mode;
                req.await.map(|m| m.id)
            }
        };
        match sent {
            Ok(id) => {
                self.message = Some(id);
                self.shown = text;
                self.last_edit = Some(Instant::now());
                true
            }
            Err(e) => {
                if let teloxide::RequestError::RetryAfter(after) = &e {
                    // Telegram named a number; keep off this chat until it
                    // passes rather than making the penalty worse.
                    self.quiet_until = Some(Instant::now() + after.duration());
                    tracing::warn!(
                        seconds = after.duration().as_secs(),
                        "flood control; pausing progress on this chat"
                    );
                }
                tracing::debug!(error = %e, "progress update failed");
                false
            }
        }
    }

    fn due(&self, now: Instant, active: usize) -> bool {
        if self.quiet_until.is_some_and(|until| now < until) {
            return false;
        }
        self.last_edit
            .is_none_or(|last| now.duration_since(last) >= edit_interval(active))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_is_tailed_at_a_word_boundary_and_html_safe() {
        assert_eq!(tail("short thought", 220), "short thought");

        let long = "a".repeat(200) + " and then the newest part of the reasoning";
        let cut = tail(&long, 30);
        assert!(cut.starts_with('…'), "got: {cut}");
        assert!(cut.ends_with("newest part of the reasoning"), "got: {cut}");
        assert!(!cut.contains("aaa"), "the old part is gone: {cut}");

        // A reasoning trace quoting markup must not become markup itself.
        assert_eq!(
            escape_html("compare <b>5 L</b> & 3 L > 1 L"),
            "compare &lt;b&gt;5 L&lt;/b&gt; &amp; 3 L &gt; 1 L"
        );
    }

    #[test]
    fn edits_are_rate_limited_but_the_first_is_immediate() {
        let mut live = Live::new(Bot::new("token"), ChatId(1), Arc::new(AtomicUsize::new(0)));
        let now = Instant::now();
        assert!(live.due(now, 1), "nothing shown yet, so the first frame goes out at once");

        live.last_edit = Some(now);
        assert!(!live.due(now + Duration::from_millis(300), 1));
        assert!(live.due(now + MIN_EDIT_INTERVAL, 1));
    }

    #[test]
    fn progress_stops_while_telegram_says_to_leave_the_chat_alone() {
        // Measured: a long run drew RetryAfter(238s) on one chat. Editing
        // through a penalty only deepens it, and the answer is what has to
        // survive — so ordinary frames stop, and a forced one still tries
        // so its caller learns it failed and can send instead.
        let mut live = Live::new(Bot::new("token"), ChatId(1), Arc::new(AtomicUsize::new(0)));
        let now = Instant::now();
        assert!(live.due(now, 1), "nothing is holding it back yet");

        live.quiet_until = Some(now + Duration::from_secs(238));
        assert!(!live.due(now + Duration::from_secs(10), 1), "still inside the penalty");
        assert!(
            live.due(now + Duration::from_secs(239), 1),
            "and it lifts once the wait Telegram named has passed"
        );
    }

    #[test]
    fn a_crowd_of_replies_paces_itself_under_telegrams_ceiling() {
        // One reply on its own is unaffected: the interval is what it always
        // was, and the answer still feels live.
        assert_eq!(edit_interval(1), MIN_EDIT_INTERVAL);
        assert_eq!(edit_interval(30), MIN_EDIT_INTERVAL, "below the crossover nothing changes");

        // A hundred replies each editing every 1.2s would be 83 edits a
        // second, nearly three times what Telegram allows one bot across all
        // of its chats. Backing off to 3.5s brings the fleet inside it.
        assert_eq!(edit_interval(100), Duration::from_millis(3500));

        // The property that actually matters, at every size: whatever the
        // load, the whole process stays under Telegram's 30 a second.
        for active in [1usize, 10, 29, 30, 31, 36, 100, 500, 5000] {
            let per_second = active as f64 / edit_interval(active).as_secs_f64();
            assert!(
                per_second <= TELEGRAM_EDITS_PER_SECOND as f64,
                "{active} streams would send {per_second:.1} edits/sec"
            );
        }
    }

    #[test]
    fn a_live_reply_counts_itself_while_it_is_in_flight() {
        let streams = Arc::new(AtomicUsize::new(0));
        {
            let _first = Live::new(Bot::new("token"), ChatId(1), streams.clone());
            assert_eq!(streams.load(Ordering::Relaxed), 1);
            let _second = Live::new(Bot::new("token"), ChatId(2), streams.clone());
            assert_eq!(streams.load(Ordering::Relaxed), 2);
        }
        // Dropped on the way out of the block — including when the handler
        // above it returned early or panicked, which is the whole reason the
        // count lives in a guard rather than in the handler.
        assert_eq!(streams.load(Ordering::Relaxed), 0, "a finished reply frees its slot");
    }
}

/// Somewhere progress can be shown.
///
/// `Live` is the real one. A test can supply a different one, which is what
/// makes the pacing and flood-control path reachable without a bot token.
///
/// Async fn in a trait, used only through generics — this is never a
/// `dyn Renderer`, so its futures needing no `Send` bound costs nothing.
///
/// Named `render*` rather than `show*` on purpose: `Live` already has
/// inherent `show`/`show_thinking`, and a trait method of the same name
/// would make `self.show(..)` inside the impl depend on inherent methods
/// winning name resolution. If that ever resolved the other way it would be
/// unbounded recursion — a stack overflow at runtime, not a compile error.
/// Different names cannot go wrong.
pub trait Renderer {
    /// Returns whether anything was actually shown.
    async fn render(&mut self, text: &str, force: bool) -> bool;
    async fn render_thinking(&mut self, text: &str) -> bool;
}

impl Renderer for Live {
    async fn render(&mut self, text: &str, force: bool) -> bool {
        self.show(text, force).await
    }
    async fn render_thinking(&mut self, text: &str) -> bool {
        self.show_thinking(text).await
    }
}

/// Draws every event the run produces, then hands the renderer back.
///
/// Ends when the channel closes, which happens when `run_agent` returns and
/// drops its sink — so the loop needs no shutdown signal of its own.
pub async fn render_events<R: Renderer>(
    mut renderer: R,
    mut events: tokio::sync::mpsc::UnboundedReceiver<scout_api::AgentEvent>,
) -> R {
    use scout_api::AgentEvent;
    while let Some(event) = events.recv().await {
        match event {
            AgentEvent::Tool(text) | AgentEvent::Answer(text) | AgentEvent::Notice(text) => {
                renderer.render(&text, false).await;
            }
            AgentEvent::Thinking(text) => {
                renderer.render_thinking(&text).await;
            }
        }
    }
    renderer
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use scout_api::AgentEvent;

    /// A renderer that writes to a list instead of to Telegram. This is the
    /// thing phase two was for: progress rendering can now be tested with no
    /// bot token, no network and no rate limiter.
    #[derive(Default)]
    struct Recorder {
        frames: Vec<(String, bool)>,
    }

    impl Renderer for Recorder {
        async fn render(&mut self, text: &str, _force: bool) -> bool {
            self.frames.push((text.to_string(), false));
            true
        }
        async fn render_thinking(&mut self, text: &str) -> bool {
            self.frames.push((text.to_string(), true));
            true
        }
    }

    #[tokio::test]
    async fn every_event_reaches_the_renderer_in_order_and_in_the_right_mode() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        scout_api::emit(&tx, AgentEvent::Tool("searching Kagi".into()));
        scout_api::emit(&tx, AgentEvent::Thinking("comparing fares".into()));
        scout_api::emit(&tx, AgentEvent::Answer("The cheapest".into()));
        scout_api::emit(&tx, AgentEvent::Notice("wrapping up".into()));
        drop(tx);

        let rec = render_events(Recorder::default(), rx).await;
        assert_eq!(
            rec.frames,
            vec![
                ("searching Kagi".to_string(), false),
                ("comparing fares".to_string(), true),
                ("The cheapest".to_string(), false),
                ("wrapping up".to_string(), false),
            ],
            "thinking renders in its own mode; everything else is ordinary text"
        );
    }

    #[tokio::test]
    async fn the_renderer_is_handed_back_when_the_run_ends() {
        // The caller needs it afterwards to send the final answer, so the
        // loop must return it rather than consume it.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        scout_api::emit(&tx, AgentEvent::Answer("done".into()));
        drop(tx);
        let rec = render_events(Recorder::default(), rx).await;
        assert_eq!(rec.frames.len(), 1);
    }

    #[tokio::test]
    async fn a_run_that_says_nothing_still_returns() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        drop(tx);
        let rec = render_events(Recorder::default(), rx).await;
        assert!(rec.frames.is_empty());
    }
}
