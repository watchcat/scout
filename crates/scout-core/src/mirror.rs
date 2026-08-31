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

use crate::core::Core;

pub use crate::store::PendingMirror;

/// Queues a thread's turns for one channel, skipping any already known.
/// Returns how many rows were written.
///
/// `delivered` is the channel saying "I have already shown the reader
/// these" — see the module's own tests, and the echo they exist to stop.
pub async fn enqueue(
    core: &Core,
    account_id: i64,
    address: &str,
    conversation_id: i64,
    turns: &[scout_api::Turn],
    delivered: bool,
) -> anyhow::Result<usize> {
    let store = core.store();
    let address = address.to_string();
    let rows: Vec<(String, String)> = turns
        .iter()
        .map(|t| (turn_key(conversation_id, t.role, &t.text), body_of(t)))
        .collect();
    let written = crate::core::blocking(move || {
        let mut written = 0;
        for (key, body) in &rows {
            if store.enqueue_mirror(account_id, TELEGRAM, &address, body, key, delivered)? {
                written += 1;
            }
        }
        Ok(written)
    })
    .await?;
    if written > 0 && !delivered {
        core.wake_mirror();
    }
    Ok(written)
}

/// How a turn reads once it is somebody else's message.
///
/// The reader's own question is quoted with a literal `>`, in plain text.
/// Not MarkdownV2: an answer is model output full of `*`, `_`, `[` and `.`,
/// every one of which MarkdownV2 requires escaped, and a missed escape turns
/// a price list into a parse error. A literal `>` reads fine and cannot fail.
fn body_of(turn: &scout_api::Turn) -> String {
    match turn.role {
        Role::You => turn.text.lines().map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n"),
        Role::Scout => turn.text.clone(),
    }
}

/// What is still waiting to go out, oldest first.
pub async fn pending(core: &Core, limit: usize) -> anyhow::Result<Vec<PendingMirror>> {
    let store = core.store();
    crate::core::blocking(move || store.pending_mirror(TELEGRAM, limit)).await
}

pub async fn sent(core: &Core, id: i64) -> anyhow::Result<()> {
    let store = core.store();
    crate::core::blocking(move || store.mark_mirror_sent(id)).await
}

pub async fn failed(core: &Core, id: i64) -> anyhow::Result<()> {
    let store = core.store();
    crate::core::blocking(move || store.mark_mirror_failed(id)).await
}

pub async fn is_enabled(core: &Core, account_id: i64) -> anyhow::Result<bool> {
    let store = core.store();
    crate::core::blocking(move || store.mirror_enabled(account_id)).await
}

pub async fn set_enabled(core: &Core, account_id: i64, on: bool) -> anyhow::Result<()> {
    let store = core.store();
    crate::core::blocking(move || store.set_mirror(account_id, on)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use scout_api::Role;

    async fn test_core() -> (crate::core::Core, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mirror.duckdb");
        let cfg = crate::config::Config::for_test(path.to_str().unwrap());
        (crate::core::Core::start(cfg, None).unwrap(), dir)
    }

    #[tokio::test]
    async fn a_thread_is_queued_once_however_often_it_is_offered() {
        // Turning the toggle on backfills; every completed run enqueues;
        // turning it off and on backfills again. One row per turn, always.
        let (core, _dir) = test_core().await;
        let account_id = crate::session::account_of(&core, crate::ids::TelegramId(4242))
            .await
            .unwrap();
        let turns = vec![
            scout_api::Turn { role: Role::You, text: "cheapest beans".to_string() },
            scout_api::Turn { role: Role::Scout, text: "here are three".to_string() },
        ];
        let queued = enqueue(&core, account_id, "4242", 1, &turns, false).await.unwrap();
        assert_eq!(queued, 2);
        let queued = enqueue(&core, account_id, "4242", 1, &turns, false).await.unwrap();
        assert_eq!(queued, 0, "the same thread was queued a second time");
        assert_eq!(pending(&core, 10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn what_telegram_already_showed_is_never_queued_for_telegram() {
        let (core, _dir) = test_core().await;
        let account_id = crate::session::account_of(&core, crate::ids::TelegramId(4242))
            .await
            .unwrap();
        let turns = vec![scout_api::Turn { role: Role::Scout, text: "here are three".to_string() }];
        enqueue(&core, account_id, "4242", 1, &turns, true).await.unwrap();
        assert!(pending(&core, 10).await.unwrap().is_empty());
        enqueue(&core, account_id, "4242", 1, &turns, false).await.unwrap();
        assert!(pending(&core, 10).await.unwrap().is_empty(), "the backfill echoed Telegram back at itself");
    }

    #[tokio::test]
    async fn queueing_something_wakes_whoever_delivers_it() {
        // The plan called this wiring untestable. It is not: `notify_one`
        // leaves a permit when nobody is waiting, so a later `notified()`
        // returns at once. Without the wake the mirror still arrives, on
        // the drain's sixty-second floor — a thread that turns up a minute
        // after you picked up your phone, which is the whole thing this
        // feature exists to avoid.
        let (core, _dir) = test_core().await;
        let account_id = crate::session::account_of(&core, crate::ids::TelegramId(4242))
            .await
            .unwrap();
        let turns = vec![scout_api::Turn { role: Role::Scout, text: "here are three".to_string() }];
        enqueue(&core, account_id, "4242", 1, &turns, false).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(50), core.mirror_waiting())
            .await
            .expect("queueing a turn did not wake the drain");
    }

    #[tokio::test]
    async fn recording_what_was_already_shown_wakes_nobody() {
        // There is nothing to deliver, so waking a drain would only spend a
        // database read to find an empty queue. Every Telegram turn takes
        // this path, so it is not a rare case.
        let (core, _dir) = test_core().await;
        let account_id = crate::session::account_of(&core, crate::ids::TelegramId(4242))
            .await
            .unwrap();
        let turns = vec![scout_api::Turn { role: Role::Scout, text: "here are three".to_string() }];
        enqueue(&core, account_id, "4242", 1, &turns, true).await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), core.mirror_waiting())
                .await
                .is_err(),
            "recording a delivered turn woke the drain for nothing"
        );
    }

    #[test]
    fn the_readers_own_words_are_quoted_line_by_line() {
        // A literal `>`, not a MarkdownV2 blockquote: the bot sends plain
        // text everywhere but two admin paths, and an unescaped `*` in an
        // answer would be a parse error rather than a price.
        let you = scout_api::Turn { role: Role::You, text: "find me\ntwo things".to_string() };
        assert_eq!(body_of(&you), "> find me\n> two things");
        let scout = scout_api::Turn { role: Role::Scout, text: "EUR 24.24 *delivered*".to_string() };
        assert_eq!(body_of(&scout), "EUR 24.24 *delivered*", "an answer must go out untouched");
    }

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
    fn what_a_channel_records_matches_what_a_backfill_would_send() {
        // The echo guarantee is two halves agreeing on a key. Telegram is
        // handed the prompt the *model* saw, which for a price request has
        // PRICE_REQUEST_NOTE appended; a backfill reads the transcript,
        // which cuts at the marker. Record the raw prompt and the keys
        // differ — so the backfill sends the reader's own question back to
        // the chat it came from, which is the one thing this design exists
        // to prevent.
        let raw = "find me cheapest gillette\n\n[system note] This is a cheapest-price request.";
        let shown = crate::text::said_by_person(raw);
        assert_eq!(
            turn_key(1, Role::You, shown),
            turn_key(1, Role::You, "find me cheapest gillette"),
            "the cut text is not what a transcript would show"
        );
        assert_ne!(
            turn_key(1, Role::You, raw),
            turn_key(1, Role::You, shown),
            "if these matched, recording the raw prompt would be harmless and this test pointless"
        );
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
