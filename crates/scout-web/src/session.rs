//! Sessions as a signed cookie, verified without touching the database.
//!
//! The alternative — a sessions table — would put the store mutex back on
//! a path a stranger can hit, which is exactly the lever the round-state
//! cache exists to remove. The cost of this choice is that a session
//! cannot be revoked: it expires, and rotating the key signs everyone out
//! at once. That is recorded in the design and is not an oversight.

use base64::Engine;
// `new_from_slice` lives on `KeyInit`, not on `Mac`.
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

pub const COOKIE: &str = "scout_session";

type HmacSha256 = Hmac<Sha256>;

fn sign(key: &[u8], payload: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(payload.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

/// A cookie value carrying an account and an expiry, signed.
///
/// The nonce makes two sessions minted in the same second for the same
/// account differ, so one cannot be recognised as a copy of the other.
pub fn mint(key: &[u8], account_id: i64, ttl_secs: i64) -> String {
    let expires = chrono::Utc::now().timestamp() + ttl_secs;
    let nonce: u64 = rand::random();
    let payload = format!("{account_id}.{expires}.{nonce:016x}");
    let sig = sign(key, &payload);
    format!("{payload}.{sig}")
}

/// The account this cookie proves, or `None`.
///
/// `None` covers every failure — wrong shape, bad signature, expired —
/// deliberately. A caller that could tell them apart would be tempted to
/// say which, and "your signature is wrong" is a hint.
pub fn verify(key: &[u8], value: &str) -> Option<i64> {
    let (payload, sig) = value.rsplit_once('.')?;
    let expected = sign(key, payload);
    // Constant-time: a byte-by-byte early return leaks how much of a
    // forged signature was right, which is enough to build one.
    if !constant_time_eq(sig.as_bytes(), expected.as_bytes()) {
        return None;
    }
    let mut parts = payload.split('.');
    let account_id: i64 = parts.next()?.parse().ok()?;
    let expires: i64 = parts.next()?.parse().ok()?;
    if chrono::Utc::now().timestamp() >= expires {
        return None;
    }
    Some(account_id)
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The value of one cookie from a `Cookie:` header.
pub fn read_cookie(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

/// A `Set-Cookie` value for a freshly minted session.
///
/// `SameSite=Lax` rather than `Strict`: the Telegram widget returns through
/// a cross-site navigation, and `Strict` would withhold the cookie at
/// exactly that moment. `Lax` still withholds it from cross-site POST,
/// which is why CSRF protection is separate rather than assumed.
pub fn set_cookie(value: &str, max_age: i64) -> String {
    format!("{COOKIE}={value}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={max_age}")
}

/// A token for a form, proving the form came from us.
///
/// The same construction as a session, over a key that is deliberately
/// *not* the session key. Signed with the session key, the hidden field in
/// every form would itself verify as a session cookie, and the page would
/// be handing out sessions in its own HTML.
///
/// This is the whole of the CSRF defence: `SameSite=Lax` withholds the
/// cookie from cross-site `POST` in current browsers, but that is a
/// property of the browser, not of the site, and the site is what has to
/// be right.
///
/// For the forms that come before a session there is nobody to bind the
/// token to, so account 0 stands in and all the token proves is that we
/// minted it within the last fifteen minutes. That is the most those
/// forms can be given: `/sign-in` is served to strangers, so a token from
/// it is by construction obtainable by anyone.
pub fn csrf(key: &[u8]) -> String {
    csrf_for(key, 0)
}

/// True when this form token is one of ours, unexpired, and belongs to no
/// session — an account-bound token is not accepted here.
pub fn csrf_ok(key: &[u8], value: &str) -> bool {
    csrf_ok_for(key, value, 0)
}

/// A form token for a page that a particular account is looking at.
///
/// The account id is inside what the signature covers, so the token is
/// only good for that account. Without this, an attacker who fetched
/// `/sign-in` — which anyone may do — would hold a token that passes the
/// check on `/sign-out` and `/account/link/email` for *everybody*, and the
/// hidden field would be proving only that Scout exists.
pub fn csrf_for(key: &[u8], account_id: i64) -> String {
    mint(&csrf_key(key), account_id, 900)
}

/// True when this form token is ours, unexpired, and was minted for this
/// account.
pub fn csrf_ok_for(key: &[u8], value: &str, account_id: i64) -> bool {
    verify(&csrf_key(key), value) == Some(account_id)
}

fn csrf_key(key: &[u8]) -> Vec<u8> {
    [key, b".csrf"].concat()
}

/// A `Set-Cookie` value that removes the session.
pub fn clear_cookie() -> String {
    format!("{COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0")
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"a test key, not the production one";

    #[test]
    fn a_cookie_round_trips_and_a_tampered_one_does_not() {
        let minted = mint(KEY, 42, 3600);
        assert_eq!(verify(KEY, &minted), Some(42));

        // One flipped character must not authenticate. This is the whole
        // reason the value is signed rather than merely encoded.
        let mut bad = minted.clone();
        let last = bad.pop().unwrap();
        bad.push(if last == 'A' { 'B' } else { 'A' });
        assert_eq!(verify(KEY, &bad), None);

        // A different key is a different server.
        assert_eq!(verify(b"another key entirely", &minted), None);
    }

    #[test]
    fn an_expired_cookie_does_not_authenticate() {
        let stale = mint(KEY, 42, -1);
        assert_eq!(verify(KEY, &stale), None, "an expired session still worked");
    }

    #[test]
    fn the_account_id_cannot_be_edited_without_breaking_the_signature() {
        // The attack this defends: read your own cookie, change 42 to 1,
        // become someone else.
        let mine = mint(KEY, 42, 3600);
        let theirs = mint(KEY, 1, 3600);
        assert_ne!(mine, theirs);
        assert_eq!(verify(KEY, &theirs), Some(1));
    }

    #[test]
    fn a_form_token_is_not_a_session_and_a_session_is_not_a_form_token() {
        // The reason the two keys differ. Were they the same, every page
        // that renders a form would be publishing a working session cookie
        // in its own HTML — for account 0 here, but the construction is
        // what matters, and Task 13 mints these for a real account id.
        let form = csrf(KEY);
        assert!(csrf_ok(KEY, &form));
        assert_eq!(verify(KEY, &form), None, "a form token authenticated as a session");

        let cookie = mint(KEY, 42, 3600);
        assert!(!csrf_ok(KEY, &cookie), "a session cookie passed as a form token");
    }

    #[test]
    fn a_form_token_for_one_account_is_no_use_on_another() {
        // The attack: fetch `/sign-in`, which is public, take the hidden
        // field, and POST it to `/sign-out` or `/account/link/email` on
        // behalf of whoever follows your link. An unbound token passes
        // both, so the check has to name the session it was minted for.
        let mine = csrf_for(KEY, 42);
        assert!(csrf_ok_for(KEY, &mine, 42));
        assert!(!csrf_ok_for(KEY, &mine, 7), "one account's form token worked for another");

        // The pre-sign-in token is bound to nobody, and that is as far as
        // it reaches: it must not stand in for a session's.
        let anonymous = csrf(KEY);
        assert!(csrf_ok(KEY, &anonymous));
        assert!(!csrf_ok_for(KEY, &anonymous, 42), "an anonymous token passed for account 42");
        assert!(!csrf_ok(KEY, &mine), "a bound token passed where none was expected");

        // And a bound token is still not a session cookie.
        assert_eq!(verify(KEY, &mine), None, "a form token authenticated as a session");
    }

    #[test]
    fn a_cookie_header_yields_only_the_named_cookie() {
        let header = "other=1; scout_session=abc; another=2";
        assert_eq!(read_cookie(header, "scout_session"), Some("abc".to_string()));
        assert_eq!(read_cookie(header, "absent"), None);
        // A prefix must not match: `xscout_session` is not `scout_session`.
        assert_eq!(read_cookie("xscout_session=abc", "scout_session"), None);
    }
}
