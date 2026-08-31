//! Sending a browser thread to the reader's Telegram chat.

use scout_api::Role;
use sha2::{Digest, Sha256};

/// What this channel is called in the outbox. One channel today; named
/// rather than written inline so a second one is a value, not an edit.
pub const TELEGRAM: &str = "telegram";

/// A stable name for one turn of one conversation.
///
/// This is what replaces a watermark. No stored message has a stable
/// identity — `replace_messages` deletes and reinserts the whole
/// conversation on every save and renumbers `position` from zero, and
/// `trim_history` drops messages off the front — so "mirrored up to here"
/// cannot be a pointer into `messages`. "Have I already sent this turn" is
/// answerable regardless, and that single change makes backfill, live
/// mirroring, and toggling the feature off and on the same operation.
///
/// sha2 rather than `DefaultHasher`: the standard hasher is explicitly not
/// stable across Rust releases, and a key that changed under a toolchain
/// upgrade would re-send every thread the reader had already read.
///
/// The `\x1f` separator is a unit separator, which cannot occur in a role
/// name and will not occur in prose. Without it, conversation 1 turn "23"
/// and conversation 12 turn "3" hash the same, and a collision here drops a
/// message with no error anywhere.
pub fn turn_key(conversation_id: i64, role: Role, text: &str) -> String {
    let role = match role {
        Role::You => "you",
        Role::Scout => "scout",
    };
    let mut hasher = Sha256::new();
    hasher.update(conversation_id.to_string().as_bytes());
    hasher.update(b"\x1f");
    hasher.update(role.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(text.as_bytes());
    // `finalize()`'s output type has no `LowerHex` impl in this sha2
    // release, so the hex encoding is done by hand, one byte at a time.
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use scout_api::Role;

    #[test]
    fn the_same_turn_always_has_the_same_key() {
        // The whole idempotence argument rests on this. If the key moved --
        // under a toolchain upgrade, say -- every thread would be sent again.
        assert_eq!(turn_key(7, Role::You, "cheapest beans"), turn_key(7, Role::You, "cheapest beans"));
        // And it is a fixed hex digest, not a hash whose stability is a
        // promise nobody made. `DefaultHasher` explicitly is not stable
        // across releases, which is why it is not used here.
        assert_eq!(turn_key(7, Role::You, "cheapest beans").len(), 64);
        assert!(turn_key(7, Role::You, "cheapest beans").chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_different_turn_has_a_different_key() {
        // Role, because "cheapest beans" asked and "cheapest beans" answered
        // are two turns. Conversation, because the same question in a new
        // thread is a new turn and has to be mirrored again.
        assert_ne!(turn_key(7, Role::You, "beans"), turn_key(7, Role::Scout, "beans"));
        assert_ne!(turn_key(7, Role::You, "beans"), turn_key(8, Role::You, "beans"));
        assert_ne!(turn_key(7, Role::You, "beans"), turn_key(7, Role::You, "rice"));
    }

    #[test]
    fn the_separator_cannot_be_moved_by_the_text() {
        // Without a separator that cannot appear in the parts, conversation
        // 1 turn "23" and conversation 12 turn "3" would collide. Contrived,
        // but a collision here silently drops somebody's message.
        assert_ne!(turn_key(1, Role::You, "23"), turn_key(12, Role::You, "3"));
    }
}
