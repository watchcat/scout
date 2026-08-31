/// Remove `<think>`/`<thinking>` blocks that reasoning models (MiniMax M3)
/// sometimes emit inline in their output. An unclosed trailing block is
/// dropped to the end of the string. Case-insensitive; result is trimmed.
///
/// Orphan closers are stripped too: when the provider streams reasoning on
/// its own channel, the text channel can begin with a bare `</think>` whose
/// opener was never in it.
pub fn strip_thinking(s: &str) -> String {
    // Openers and their contents are gone by here; `strip_thinking_blocks`
    // drops an unclosed one all the way to the end. What can survive is a
    // closer with no opener, which MiniMax streams once per turn when the
    // reasoning went to its own channel and only the tag came through the
    // text one.
    //
    // Such a closer means the text began *inside* a thinking block, so
    // everything in front of it is reasoning and goes with it. Removing the
    // tag and keeping what preceded it published a whole chain of thought
    // into a chat once, system prompt and all.
    let stripped = strip_thinking_blocks(s);
    let mut out = stripped.as_str();
    loop {
        let lower = out.to_ascii_lowercase();
        // Prefer the longer closer at the same position ("</thinking>" has
        // "</think>" as a prefix), and take the earliest one so the answer
        // is whatever follows the last of them.
        let closer = ["</thinking>", "</think>"]
            .iter()
            .filter_map(|t| lower.find(t).map(|i| (i, t.len())))
            .min_by_key(|&(i, len)| (i, std::cmp::Reverse(len)));
        match closer {
            Some((i, len)) => out = &out[i + len..],
            None => break,
        }
    }
    out.trim().to_string()
}

fn strip_thinking_blocks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        let lower = rest.to_ascii_lowercase();
        let Some(start) = lower.find("<think") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        // Prefer the longer closer when both match at the same position
        // ("</thinking>" contains "</think>" as a prefix).
        let closer = ["</thinking>", "</think>"]
            .iter()
            .filter_map(|t| lower[start..].find(t).map(|i| (start + i, t.len())))
            .min_by_key(|&(i, len)| (i, std::cmp::Reverse(len)));
        match closer {
            Some((i, len)) => rest = &rest[i + len..],
            None => break, // unclosed block: drop the remainder
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_thinking_removes_closed_blocks() {
        assert_eq!(
            strip_thinking("<think>hmm what is this</think>Alfa AWUS036ACHM wifi adapter"),
            "Alfa AWUS036ACHM wifi adapter"
        );
        assert_eq!(
            strip_thinking("<thinking>let me look</thinking>red bike<thinking>more</thinking> 26 inch"),
            "red bike 26 inch"
        );
    }

    #[test]
    fn strip_thinking_prefers_matching_longer_closer() {
        // "</thinking>" must be consumed whole, not just its "</think>" prefix.
        assert_eq!(strip_thinking("<thinking>x</thinking>query"), "query");
        assert!(!strip_thinking("<thinking>x</thinking>query").contains("ing>"));
    }

    #[test]
    fn strip_thinking_drops_unclosed_trailing_block() {
        assert_eq!(strip_thinking("good query <think>and then it ramble"), "good query");
    }

    #[test]
    fn strip_thinking_removes_orphan_closers() {
        // What MiniMax actually streamed once reasoning moved to its own
        // channel: closers with no opener, one per model turn.
        assert_eq!(
            strip_thinking("</think>\n\n</think>\n\nI need to clarify which products."),
            "I need to clarify which products."
        );
        assert_eq!(strip_thinking("</thinking>answer"), "answer");
        // a stray opener with nothing after it leaves nothing behind
        assert_eq!(strip_thinking("<think>"), "");
    }

    #[test]
    fn text_before_an_unmatched_closer_is_reasoning_and_goes_with_it() {
        // Measured: a whole chain of thought reached the chat — "Actually
        // wait, let me re-read the system prompt..." and the system prompt
        // quoted back at the traveller. The stream had begun inside a
        // thinking block, so only the closing tag arrived, and dropping the
        // tag while keeping everything in front of it published the lot.
        //
        // A closer with no opener means the text started mid-thought.
        // Everything before it is reasoning.
        assert_eq!(strip_thinking("my reasoning</think>The answer"), "The answer");
        assert_eq!(
            strip_thinking("step one\nstep two</think>\n\nHere are your flights."),
            "Here are your flights."
        );
        // Later closers win: the answer is whatever follows the last one.
        assert_eq!(strip_thinking("a</think>b</think>real answer"), "real answer");
        // Nothing but reasoning leaves nothing, which is the right answer —
        // there was no answer in it.
        assert_eq!(strip_thinking("only ever thinking</think>"), "");
    }

    #[test]
    fn strip_thinking_leaves_plain_text_alone() {
        assert_eq!(strip_thinking("  plain query  "), "plain query");
    }

    #[test]
    fn a_client_fed_updates_sees_exactly_what_strip_thinking_says_at_every_step() {
        // The load-bearing property. A stream arrives one character at a
        // time; at every prefix, a client that applied the updates must be
        // showing precisely what `strip_thinking` would have shown. The
        // second case is the one that matters: a closer with no opener
        // means everything before it was reasoning, so the client has to be
        // told to throw away what it already displayed.
        for source in [
            "Here are three bikes<think>actually let me reconsider",
            "secret reasoning here</think>The answer",
        ] {
            let mut shown = scout_api::Shown::default();
            let mut client = String::new();
            for (i, _) in source.char_indices().chain(std::iter::once((source.len(), ' '))) {
                let answer = strip_thinking(&source[..i]);
                if let Some(update) = shown.update(&answer) {
                    update.apply(&mut client);
                }
                assert_eq!(
                    client, answer,
                    "client drifted at prefix {i:?} of {source:?}"
                );
            }
        }
    }

    #[test]
    fn a_stray_closer_retracts_what_was_already_shown() {
        // Named separately because this is the security property, not a
        // formatting one: without the Replace the client keeps the
        // reasoning on screen.
        let source = "secret reasoning here</think>The answer";
        let mut shown = scout_api::Shown::default();
        // Everything up to the closer is shown as answer text.
        assert!(matches!(
            shown.update(&strip_thinking(&source[..21])),
            Some(scout_api::TextUpdate::Append(ref t)) if t == "secret reasoning here"
        ));
        // The completed closer retracts all of it.
        assert_eq!(
            shown.update(&strip_thinking(&source[..29])),
            Some(scout_api::TextUpdate::Replace(String::new())),
            "the retraction was not sent, so a client would still be showing reasoning"
        );
    }
}
