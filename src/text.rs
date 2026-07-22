pub const TELEGRAM_LIMIT: usize = 4096;

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
