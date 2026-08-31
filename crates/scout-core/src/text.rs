/// The marker that introduces text Scout addressed to itself.
///
/// Instructions handed to the model mid-conversation — "these links are
/// dead, re-send your answer" — have to ride along as user messages,
/// because a user message is the only way to say something to a model
/// between turns. `rig`'s `Chat::chat` then appends the prompt it was given
/// to the history, so those notes are saved and, until this existed, were
/// rendered back to the person as things *they* had said.
///
/// The producers spell it literally; `a_note_scout_writes_to_itself_is_marked`
/// is what stops the spelling drifting apart from this one.
pub const SYSTEM_NOTE: &str = "[system note]";

/// What a person actually said, with any note Scout appended to itself cut
/// away. Empty when the message was nothing but a note.
///
/// Cutting at the marker rather than matching a whole prompt: two of the
/// four notes are appended to a real question, so the person's words come
/// first and have to survive.
pub fn said_by_person(text: &str) -> &str {
    match text.find(SYSTEM_NOTE) {
        Some(i) => text[..i].trim(),
        None => text.trim(),
    }
}

/// Remove `<think>`/`<thinking>` blocks that reasoning models (MiniMax M3)
/// sometimes emit inline in their output. An unclosed trailing block is
/// dropped to the end of the string. Case-insensitive; result is trimmed.
///
/// Orphan closers are stripped too: when the provider streams reasoning on
/// its own channel, the text channel can begin with a bare `</think>` whose
/// opener was never in it.
///
/// An XML namespace prefix on the tag is ignored, so `</mm:think>` is the
/// same tag as `</think>`.
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
    let stripped = strip_thinking_blocks(&unnamespace_think_tags(s));
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

/// Rewrites `<mm:think>` to `<think>`, and any other namespace prefix on a
/// thinking tag likewise, so that everything below matches two spellings
/// rather than unboundedly many.
///
/// A prefix rather than a wider tag search because the two are not the same
/// risk. Matching `think` anywhere inside a tag name would swallow markup
/// nobody meant as reasoning; dropping a `ns:` that sits immediately in
/// front of `think` cannot, because the rest of the name still has to match.
///
/// Byte indices are safe here: every byte this inspects is ASCII, and an
/// ASCII byte never occurs inside a multi-byte character, so each index it
/// slices at is a character boundary.
fn unnamespace_think_tags(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    // Everything before this has been copied out already.
    let mut copied = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        // A closer's slash belongs to the tag, not to the prefix.
        let name = if b.get(i + 1) == Some(&b'/') { i + 2 } else { i + 1 };
        let mut end = name;
        while end < b.len() && (b[end].is_ascii_alphanumeric() || matches!(b[end], b'_' | b'-' | b'.')) {
            end += 1;
        }
        // A prefix is a non-empty name followed by a colon, and it only
        // counts as one when `think` is what comes after the colon.
        let is_prefixed_think = end > name
            && b.get(end) == Some(&b':')
            && b[end + 1..].len() >= 5
            && b[end + 1..end + 6].eq_ignore_ascii_case(b"think");
        if is_prefixed_think {
            out.push_str(&s[copied..name]);
            copied = end + 1;
        }
        i += 1;
    }
    out.push_str(&s[copied..]);
    out
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
    fn a_note_scout_wrote_to_itself_is_not_something_the_person_said() {
        // The repair prompt is a whole message and leaves nothing behind.
        assert_eq!(said_by_person(&crate::links::repair_prompt(&["https://x/404".into()])), "");
        // The price note rides on the end of a real question, which stays.
        assert_eq!(
            said_by_person("find me cheapest gillette\n\n[system note] This is a cheapest-price request."),
            "find me cheapest gillette"
        );
        assert_eq!(said_by_person("  an ordinary question  "), "an ordinary question");
    }

    #[test]
    fn a_note_scout_writes_to_itself_is_marked() {
        // The cut above only works while every producer spells the marker
        // the same way. `const` cannot be built from a `const` with
        // `concat!`, so the producers hold literals and this is what stops
        // them drifting.
        assert!(crate::links::repair_prompt(&["https://x/404".into()]).starts_with(SYSTEM_NOTE));
        assert!(crate::agent::WRAP_UP_NOTE.starts_with(SYSTEM_NOTE));
        assert!(crate::toolcall::REPAIR_NOTE.starts_with(SYSTEM_NOTE));
    }

    #[test]
    fn strip_thinking_leaves_plain_text_alone() {
        assert_eq!(strip_thinking("  plain query  "), "plain query");
    }

    #[test]
    fn a_namespaced_tag_is_still_a_thinking_tag() {
        // MiniMax began namespacing the tag in August 2026. `</mm:think>`
        // matched neither the opener search nor either closer, so it was
        // not stripped and — worse — the rule that everything in front of
        // an orphan closer is reasoning never fired. Measured on a real
        // transcript: a page of deliberation reached the chat, tag and all.
        assert_eq!(strip_thinking("my reasoning</mm:think>The answer"), "The answer");
        assert_eq!(strip_thinking("<mm:think>hidden</mm:think>The answer"), "The answer");
        assert_eq!(strip_thinking("<mm:thinking>hidden</mm:thinking>answer"), "answer");
        // Later closers still win, whatever they are spelled.
        assert_eq!(strip_thinking("a</think>b</mm:think>real answer"), "real answer");
        // An unclosed namespaced opener drops the rest, like its bare twin.
        assert_eq!(strip_thinking("good query <mm:think>and then it rambles"), "good query");
    }

    #[test]
    fn a_namespace_is_only_dropped_from_thinking_tags() {
        // The rewrite is aimed at one tag name. Markup that merely carries a
        // prefix has to survive it, or stripping reasoning would start
        // quietly editing the answer.
        assert_eq!(strip_thinking("see <ns:item>one</ns:item>"), "see <ns:item>one</ns:item>");
        assert_eq!(strip_thinking("a<b:c>d"), "a<b:c>d");
        assert_eq!(strip_thinking("time is 10<12:30"), "time is 10<12:30");
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
            "Here are three bikes<mm:think>actually let me reconsider",
            "secret reasoning here</mm:think>The answer",
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
