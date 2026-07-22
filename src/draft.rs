/// What to do with an incoming text message given the chat's pending
/// photo-draft (if any).
#[derive(Debug, PartialEq)]
pub enum DraftResolution {
    /// No pending draft — treat the text as a normal message.
    NoDraft,
    /// User confirmed the draft — search with it.
    Confirmed(String),
    /// User sent their own text instead — search with that.
    Replaced(String),
    /// User dismissed the draft — drop it, search nothing.
    Cancelled,
}

/// Matches the reply-keyboard "Go" button as well as typed confirmations.
const CONFIRM_WORDS: &[&str] = &["go", "ok", "okay", "yes", "y", "sure"];
/// Matches the reply-keyboard "Cancel" button as well as typed dismissals.
const CANCEL_WORDS: &[&str] = &["cancel", "no", "stop", "nevermind", "never mind"];

pub fn resolve_draft(pending: Option<&str>, text: &str) -> DraftResolution {
    let Some(draft) = pending else {
        return DraftResolution::NoDraft;
    };
    let normalized = text.trim().to_lowercase();
    if CONFIRM_WORDS.contains(&normalized.as_str()) {
        DraftResolution::Confirmed(draft.to_string())
    } else if CANCEL_WORDS.contains(&normalized.as_str()) {
        DraftResolution::Cancelled
    } else {
        DraftResolution::Replaced(text.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_pending_draft_passes_through() {
        assert_eq!(resolve_draft(None, "find me a bike"), DraftResolution::NoDraft);
    }

    #[test]
    fn confirm_words_use_the_draft() {
        for word in ["go", "GO", " ok ", "Yes", "y", "sure", "Go"] {
            assert_eq!(
                resolve_draft(Some("red mountain bike"), word),
                DraftResolution::Confirmed("red mountain bike".to_string()),
                "word: {word:?}"
            );
        }
    }

    #[test]
    fn cancel_words_dismiss_the_draft() {
        for word in ["cancel", "Cancel", " no ", "STOP", "nevermind", "Never mind"] {
            assert_eq!(
                resolve_draft(Some("red mountain bike"), word),
                DraftResolution::Cancelled,
                "word: {word:?}"
            );
        }
    }

    #[test]
    fn cancel_words_without_draft_are_normal_text() {
        assert_eq!(resolve_draft(None, "cancel"), DraftResolution::NoDraft);
    }

    #[test]
    fn other_text_replaces_the_draft() {
        assert_eq!(
            resolve_draft(Some("red mountain bike"), "blue city bike, max 200 EUR"),
            DraftResolution::Replaced("blue city bike, max 200 EUR".to_string())
        );
    }
}
