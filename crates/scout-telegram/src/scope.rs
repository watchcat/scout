//! Which conversation a Telegram update belongs to.
//!
//! The rule is Telegram-shaped — it reads a chat id against a user id — so it
//! lives here. The strings it produces are core's vocabulary: `direct` is the
//! thread the web client will share, which is why the scope column has said
//! so since phase one.

/// In a 1:1 chat Telegram makes the chat id equal the user id, and that is
/// the thread the web app will share. Anywhere else is a room with other
/// people in it and keeps its own history.
pub fn conversation_scope(chat_id: i64, user_id: i64) -> String {
    if chat_id == user_id {
        "direct".to_string()
    } else {
        format!("telegram:{chat_id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scope_of_a_private_chat_is_shared_and_a_group_is_not() {
        // Telegram makes chat id equal user id in a 1:1 chat, and that
        // thread is the one the web app will share. A group is a room with
        // other people in it and keeps its own history.
        assert_eq!(conversation_scope(4242, 4242), "direct");
        assert_eq!(conversation_scope(-100123, 4242), "telegram:-100123");
    }
}
