//! Verifying the Telegram Login Widget's payload.
//!
//! This lives in `scout-web` rather than `scout-telegram` because it is
//! arithmetic, not Telegram integration: HMAC over sorted fields with
//! `SHA256(bot_token)` as the key. Keeping it here means `scout-web` needs
//! no dependency on the Telegram adapter, which the design requires, and
//! it is testable against a fixture with no network.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

/// How stale a signed payload may be. It stays validly signed forever, so
/// without this a payload captured from a URL bar, a proxy log or a browser
/// history is a permanent key to that account.
const MAX_AGE_SECS: i64 = 60;

/// The Telegram user id this payload proves, or `None`.
pub fn verify(bot_token: &str, fields: &[(String, String)]) -> Option<i64> {
    let given = fields.iter().find(|(k, _)| k == "hash")?.1.clone();

    let mut rest: Vec<&(String, String)> = fields.iter().filter(|(k, _)| k != "hash").collect();
    rest.sort_by(|a, b| a.0.cmp(&b.0));
    let check = rest.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n");

    let secret = Sha256::digest(bot_token.as_bytes());
    let mut mac = Hmac::<Sha256>::new_from_slice(&secret).ok()?;
    mac.update(check.as_bytes());
    let expected: String =
        mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect();

    if !crate::session::constant_time_eq(given.as_bytes(), expected.as_bytes()) {
        return None;
    }

    let auth_date: i64 = rest.iter().find(|(k, _)| k == "auth_date")?.1.parse().ok()?;
    if chrono::Utc::now().timestamp() - auth_date > MAX_AGE_SECS {
        return None;
    }
    rest.iter().find(|(k, _)| k == "id")?.1.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "123456:test-bot-token";

    /// Builds a payload signed the way Telegram signs one, so the test
    /// exercises the real algorithm rather than our own idea of it.
    fn signed(fields: &[(&str, &str)]) -> Vec<(String, String)> {
        use hmac::{Hmac, KeyInit, Mac};
        use sha2::{Digest, Sha256};
        let mut pairs: Vec<(String, String)> =
            fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        let check = pairs.iter().map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>().join("\n");
        let secret = Sha256::digest(TOKEN.as_bytes());
        let mut mac = Hmac::<Sha256>::new_from_slice(&secret).unwrap();
        mac.update(check.as_bytes());
        let hash = mac.finalize().into_bytes().iter()
            .map(|b| format!("{b:02x}")).collect::<String>();
        pairs.push(("hash".to_string(), hash));
        pairs
    }

    fn now() -> i64 { chrono::Utc::now().timestamp() }

    #[test]
    fn a_genuine_payload_yields_the_telegram_id() {
        let auth = now().to_string();
        let p = signed(&[("id", "777"), ("first_name", "Ada"), ("auth_date", &auth)]);
        assert_eq!(verify(TOKEN, &p), Some(777));
    }

    #[test]
    fn a_mutated_field_is_refused() {
        let auth = now().to_string();
        let mut p = signed(&[("id", "777"), ("first_name", "Ada"), ("auth_date", &auth)]);
        // Become user 1 by editing the id the signature covers.
        for pair in p.iter_mut() {
            if pair.0 == "id" { pair.1 = "1".to_string(); }
        }
        assert_eq!(verify(TOKEN, &p), None, "an edited id was accepted");
    }

    #[test]
    fn a_stale_payload_is_refused_even_though_it_is_genuine() {
        // Replay: a payload captured from a URL bar or a log stays validly
        // signed forever. auth_date is the only thing that expires it.
        let old = (now() - 3600).to_string();
        let p = signed(&[("id", "777"), ("first_name", "Ada"), ("auth_date", &old)]);
        assert_eq!(verify(TOKEN, &p), None, "an hour-old sign-in was replayed");
    }

    #[test]
    fn a_payload_with_no_hash_is_refused() {
        let auth = now().to_string();
        assert_eq!(
            verify(TOKEN, &[("id".into(), "777".into()), ("auth_date".into(), auth)]),
            None
        );
    }
}
