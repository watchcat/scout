pub const TELEGRAM_LIMIT: usize = 4096;

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

/// Split `text` into chunks of at most `limit` characters, preferring to cut
/// at newlines, then spaces, so URLs and words stay intact.
///
/// Panics if limit == 0.
pub fn split_message(text: &str, limit: usize) -> Vec<String> {
    assert!(limit > 0, "limit must be > 0");
    let mut chunks = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        if rest.chars().count() <= limit {
            chunks.push(rest.to_string());
            break;
        }
        // Byte index just past the limit-th char: cuts must land at or before it.
        let hard_end = rest
            .char_indices()
            .nth(limit)
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let window = &rest[..hard_end];
        let cut = window
            .rfind('\n')
            .or_else(|| window.rfind(' '))
            .filter(|&i| i > 0)
            .unwrap_or(hard_end);
        chunks.push(window[..cut].trim_end().to_string());
        rest = rest[cut..].trim_start();
    }
    chunks.retain(|c| !c.is_empty());
    chunks
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
    fn short_text_is_one_chunk() {
        assert_eq!(split_message("hello", 10), vec!["hello"]);
    }

    #[test]
    fn empty_text_yields_no_chunks() {
        assert!(split_message("  ", 10).is_empty());
    }

    #[test]
    fn splits_at_newline_before_limit() {
        let text = "first line\nsecond line";
        assert_eq!(split_message(text, 15), vec!["first line", "second line"]);
    }

    #[test]
    fn splits_at_space_when_no_newline() {
        let text = "one two three four";
        assert_eq!(split_message(text, 9), vec!["one two", "three", "four"]);
    }

    #[test]
    fn unbroken_run_is_hard_cut() {
        let text = "a".repeat(25);
        let chunks = split_message(&text, 10);
        assert_eq!(chunks, vec!["a".repeat(10), "a".repeat(10), "a".repeat(5)]);
    }

    #[test]
    #[should_panic]
    fn zero_limit_panics() {
        split_message("hello", 0);
    }

    #[test]
    fn multibyte_text_splits_on_char_boundaries() {
        let text = "żółć ".repeat(10); // 50 chars, mostly multibyte
        let chunks = split_message(&text, 12);
        for c in &chunks {
            assert!(c.chars().count() <= 12, "chunk too long: {c:?}");
        }
        assert_eq!(chunks.join(" ").split_whitespace().count(), 10);
    }

    #[test]
    fn no_chunk_exceeds_limit_and_nothing_is_lost() {
        let text = "para one with a url https://example.com/very/long/path\n\n\
                    para two is here and also somewhat long\n\
                    para three"
            .repeat(20);
        let chunks = split_message(&text, 100);
        for c in &chunks {
            assert!(c.chars().count() <= 100, "chunk too long: {c:?}");
        }
        // URL survives unsplit in every chunk that contains it
        for c in &chunks {
            if c.contains("https://") {
                assert!(c.contains("https://example.com/very/long/path"));
            }
        }
    }
}
