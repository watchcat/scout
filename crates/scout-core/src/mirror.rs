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
/// The `\x1f` separators are insurance against a change to these fields,
/// not a fix for a collision that exists today. Measured: with this exact
/// layout the concatenation is already injective, because the id is digits,
/// a role starts with a letter, and neither role name is a prefix of the
/// other — so `1|you|23` and `12|you|3` cannot be confused even unseparated.
///
/// The first draft of this claimed otherwise, and shipped a test that
/// asserted the two keys differ. They do, with or without the separators,
/// so the test passed under its own mutation and could never have failed.
/// The separators stay because hashing concatenated fields without them is
/// a habit that bites the first time a field stops being digits.
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
    fn neighbouring_fields_cannot_be_confused_for_one_another() {
        // Kept because the property is worth holding, stated as what it is:
        // these differ because the layout is unambiguous, and they would
        // differ without the separators too. `a_different_turn_has_a
        // _different_key` is what actually guards the key's job.
        assert_ne!(turn_key(1, Role::You, "23"), turn_key(12, Role::You, "3"));
        assert_ne!(turn_key(1, Role::Scout, "x"), turn_key(1, Role::You, "scoutx"));
    }
}
