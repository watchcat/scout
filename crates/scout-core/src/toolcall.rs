//! When the model writes a tool call instead of making one.
//!
//! Observed on minimax-m3 at turn 4 of 20: the reply after `</think>` was
//! `<br><tool_call>\n<invoke name="fetch_page">...` in plain text. `rig`
//! sees a response with no structured call in it, concludes the agent has
//! finished, and hands the markup back as the answer — so nothing errors,
//! nothing is logged, and the reader gets XML.
//!
//! The syntax is the tell: `<invoke name="...">` is Anthropic's calling
//! convention, not this provider's, so the model had dropped out of its own
//! format rather than misusing it.

/// The markers of a call written as prose.
///
/// Tag openers only. A bare word like "invoke" appears in ordinary prose,
/// and this runs against every answer the bot writes.
const MARKERS: &[&str] = &["<tool_call", "<invoke name=", "<function_call", "<tool_use"];

/// Whether a finished reply is really a tool call the model failed to make.
///
/// Deliberately a low bar: a false positive costs one corrective turn and
/// the model then answers normally, while a false negative puts markup on
/// the reader's screen. Only a reply that comes back as markup *twice* is
/// treated as a failure, and no real answer does that.
pub fn looks_like_tool_call(reply: &str) -> bool {
    let lower = reply.to_ascii_lowercase();
    MARKERS.iter().any(|m| lower.contains(m))
}

/// The follow-up handed to the agent when it wrote a call instead of making
/// one. Opens with the marker so `session::turns_of` never renders it as
/// something the reader said.
pub const REPAIR_NOTE: &str = "[system note] Your last reply contained a tool call written out \
as text instead of an actual tool call, so no tool ran and the user saw markup. Either make the \
call properly now, or answer from what you have already found. Reply with the answer only — no \
markup, no apology, and no explanation of this note.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_markup_that_reached_a_reader_is_recognised() {
        // Verbatim from the log, after `strip_thinking` had removed the
        // reasoning that preceded it.
        let reply = "<br><tool_call>\n\
             <invoke name=\"fetch_page\"><url>https://www.kruidvat.nl/x/p/5941598</url></invoke>\n\
             <invoke name=\"search_web\"><query>Kruidvat OneBlade 2+1</query></invoke>\n\
             </tool_call>";
        assert!(looks_like_tool_call(reply));
    }

    #[test]
    fn an_ordinary_answer_is_not_a_tool_call() {
        // Every answer this bot writes goes through here, and most of them
        // carry urls, prices and the odd angle bracket.
        assert!(!looks_like_tool_call(
            "EUR 24.24 delivered, bol.com (2 blades, EUR 12.12/blade)\n\
             https://www.bol.com/nl/nl/p/philips-oneblade-qp220-50/9200000073215416/"
        ));
        assert!(!looks_like_tool_call("I could not invoke the shop's search, so I compared 3 < 5 offers by hand."));
        assert!(!looks_like_tool_call(""));
    }

    #[test]
    fn the_note_is_marked_as_scouts_own() {
        // Rig appends this prompt to the history, so without the marker the
        // reader would see it in their transcript as their own words.
        assert!(REPAIR_NOTE.starts_with(crate::text::SYSTEM_NOTE));
        assert_eq!(crate::text::said_by_person(REPAIR_NOTE), "");
    }
}
