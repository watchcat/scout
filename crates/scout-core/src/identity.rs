//! Who an account is, and how a second way of proving it gets attached.
//!
//! Everything here is keyed on `account_id` and knows nothing about
//! cookies, requests or email delivery. That is what lets W4 turn these
//! into API calls without changing their shape.

use crate::core::{blocking, Core};

/// Re-exported so the web layer can name these without reaching into
/// `store`, which is private.
pub use crate::store::{Claim, LinkOutcome, TokenOutcome};

/// What signing in produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignIn {
    /// Admitted to a round just now, or already a member.
    In { account_id: i64 },
    /// Not in. Either the rounds are full, or there are none open.
    ///
    /// Not a promise of a waitlist row: a revoked account is folded into
    /// this same outcome (see `claim_seat`'s comment on `Claim::Revoked`),
    /// and a revoked account is turned away before a waitlist row is ever
    /// written. A page built on this must never claim a queue position it
    /// cannot actually produce.
    Queued { account_id: i64 },
}

/// Resolves the identity to an account, then tries to seat it in the
/// newest open round with room.
///
/// With no round to seat it in, it stops at resolving the account rather
/// than calling `claim_seat` — that call needs a code to file a waitlist
/// row under, and with no rounds at all there is none.
pub async fn sign_in(core: &Core, kind: &'static str, external_id: &str) -> anyhow::Result<SignIn> {
    let store = core.store();
    let external_id = external_id.to_string();
    blocking(move || {
        let account_id = store.account_for_identity(kind, &external_id)?;

        // `rounds()` comes back oldest first, so searching from the back
        // finds the newest round that will still take someone — the same
        // selection `Core::admission` makes, so the page and sign-in never
        // disagree about which round is current.
        let newest_with_room =
            store.rounds()?.into_iter().rfind(|r| r.open && r.used < r.capacity);

        let Some(round) = newest_with_room else {
            return Ok(SignIn::Queued { account_id });
        };

        let claim = store.claim_seat(account_id, &round.code)?;
        Ok(match claim {
            Claim::Admitted | Claim::AlreadyIn => SignIn::In { account_id },
            Claim::NoRoom | Claim::Revoked => SignIn::Queued { account_id },
        })
    })
    .await
}

/// Where an account stands, and how it can prove itself.
///
/// Both facts in one struct because one page wants both and each is a turn
/// of the store's mutex. `Queued` is the absence of a seat rather than a
/// waitlist row, for the reason `SignIn::Queued` gives: a revoked account
/// has no seat and no row either, and neither this type nor a page built
/// on it may claim a queue position it cannot produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Standing {
    pub member: bool,
    /// `'email'`, `'telegram'` — sorted, so a page renders the same way
    /// twice, and each named once however many identities of that kind the
    /// account holds. See `Store::identity_kinds`.
    pub kinds: Vec<String>,
}

/// Reads an account without changing it.
pub async fn standing(core: &Core, account_id: i64) -> anyhow::Result<Standing> {
    let store = core.store();
    blocking(move || {
        Ok(Standing {
            member: store.is_member(account_id)?,
            kinds: store.identity_kinds(account_id)?,
        })
    })
    .await
}

/// Attaches a second way of proving the same account.
pub async fn link(
    core: &Core,
    account_id: i64,
    kind: &'static str,
    external_id: &str,
) -> anyhow::Result<LinkOutcome> {
    let store = core.store();
    let owned = external_id.to_string();
    let outcome = blocking(move || store.link_identity(account_id, kind, &owned)).await?;

    // A merge moves the absorbed account's seat onto the survivor whenever
    // the survivor has none. That is right for an ordinary account, whose
    // only admission may have arrived that way — and wrong for a founder,
    // who is admitted by `ALLOWED_TELEGRAM_USER_IDS` and has no `members`
    // row on purpose. Left alone it makes a round report a seat spent that
    // nobody is sitting in, which is the number admissions are decided on.
    //
    // Here rather than in `link_identity` because the founder list is
    // configuration the store has never been told about, and telling it
    // would make admission a second thing the database has an opinion on.
    if let LinkOutcome::Merged { account_id: survivor } = outcome {
        let store = core.store();
        let ids = blocking(move || store.telegram_ids(survivor)).await?;
        if ids.iter().any(|id| core.is_founder(*id)) {
            let store = core.store();
            blocking(move || store.release_seat(survivor)).await?;
        }
    }
    Ok(outcome)
}

/// Files a login token, ready to be emailed.
pub async fn issue_token(
    core: &Core,
    token_hash: &str,
    email: &str,
    account_id: Option<i64>,
    ttl_secs: i64,
) -> anyhow::Result<()> {
    let store = core.store();
    let token_hash = token_hash.to_string();
    let email = email.to_string();
    blocking(move || store.issue_login_token(&token_hash, &email, account_id, ttl_secs)).await
}

/// Spends a login token, if it has anything left to spend.
pub async fn consume_token(core: &Core, token_hash: &str) -> anyhow::Result<TokenOutcome> {
    let store = core.store();
    let token_hash = token_hash.to_string();
    blocking(move || store.consume_login_token(&token_hash)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    async fn test_core() -> (Core, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.duckdb");
        let cfg = Config::for_test(path.to_str().unwrap());
        let core = Core::start(cfg, None).unwrap();
        (core, dir)
    }

    #[tokio::test]
    async fn signing_in_at_an_open_round_admits_and_makes_a_member() {
        let (core, _dir) = test_core().await;
        core.store().create_round("autumn", 1).unwrap();

        let outcome = sign_in(&core, "email", "new@example.com").await.unwrap();

        let SignIn::In { account_id } = outcome else {
            panic!("expected In, got {outcome:?}");
        };
        // `claim_seat` checks membership before the round, so asking again
        // for the one now-full seat still comes back `AlreadyIn` rather
        // than `NoRoom` — that is the store's own proof of membership.
        assert_eq!(core.store().claim_seat(account_id, "autumn").unwrap(), Claim::AlreadyIn);
    }

    #[tokio::test]
    async fn signing_in_when_every_round_is_full_is_queued() {
        let (core, _dir) = test_core().await;
        core.store().create_round("autumn", 1).unwrap();
        // Fill the one seat with somebody else first.
        let filler = core.store().account_for_identity("email", "filler@example.com").unwrap();
        core.store().claim_seat(filler, "autumn").unwrap();

        let outcome = sign_in(&core, "email", "late@example.com").await.unwrap();

        assert!(matches!(outcome, SignIn::Queued { .. }));
    }

    #[tokio::test]
    async fn signing_in_with_no_rounds_at_all_is_queued_and_does_not_panic() {
        let (core, _dir) = test_core().await;

        let outcome = sign_in(&core, "email", "nobody@example.com").await.unwrap();

        assert!(matches!(outcome, SignIn::Queued { .. }));
    }

    #[tokio::test]
    async fn signing_in_twice_with_the_same_identity_is_the_same_account() {
        let (core, _dir) = test_core().await;
        core.store().create_round("autumn", 5).unwrap();

        let first = sign_in(&core, "email", "same@example.com").await.unwrap();
        let second = sign_in(&core, "email", "same@example.com").await.unwrap();

        let (SignIn::In { account_id: a }, SignIn::In { account_id: b }) = (first, second) else {
            panic!("expected both In");
        };
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn link_attaches_a_second_identity_and_refuses_one_owned_by_another() {
        let (core, _dir) = test_core().await;
        let account_id = core.store().account_for_identity("email", "owner@example.com").unwrap();
        let other_id = core.store().account_for_identity("email", "other@example.com").unwrap();
        // Both have used Scout, so neither may be absorbed into the other.
        // Two empty accounts are one person twice and would merge — see
        // `store`'s tests for that path.
        core.store().log_request(account_id, "text").unwrap();
        core.store().log_request(other_id, "text").unwrap();

        let outcome = link(&core, account_id, "telegram", "12345").await.unwrap();
        assert_eq!(outcome, LinkOutcome::Linked);

        // Somebody else already proved "telegram:12345" — the account it
        // belongs to must not change.
        let taken = link(&core, other_id, "telegram", "12345").await.unwrap();
        assert_eq!(taken, LinkOutcome::TakenByAnother);
        assert_eq!(
            core.store().account_for_identity("telegram", "12345").unwrap(),
            account_id
        );
    }

    #[tokio::test]
    async fn a_founder_absorbing_an_empty_account_does_not_inherit_its_seat() {
        // `Config::for_test` makes telegram 111 the founder. A founder is
        // admitted by the allow-list and holds no `members` row, so a seat
        // that lands on one is a seat the round has lost for nobody.
        let (core, _dir) = test_core().await;
        core.store().create_round("autumn", 5).unwrap();
        let founder = core.store().account_for_identity("telegram", "111").unwrap();
        core.store().log_request(founder, "text").unwrap();
        assert!(!core.store().is_member(founder).unwrap(), "a founder started with a seat");

        // Signing in on the web mints an empty account, which takes a seat.
        let SignIn::In { account_id: web } =
            sign_in(&core, "email", "f@example.com").await.unwrap()
        else {
            panic!("the round had room");
        };
        assert_eq!(core.store().rounds().unwrap()[0].used, 1);

        assert_eq!(
            link(&core, web, "telegram", "111").await.unwrap(),
            LinkOutcome::Merged { account_id: founder }
        );
        assert_eq!(
            core.store().rounds().unwrap()[0].used,
            0,
            "the founder inherited a seat instead of handing it back"
        );
        assert!(!core.store().is_member(founder).unwrap());
    }

    #[tokio::test]
    async fn an_ordinary_account_does_inherit_the_seat_it_was_merged_with() {
        // The other side of the same rule. Handing this seat back would
        // sign somebody in and then throw them out, because unlike a
        // founder their admission is the seat.
        let (core, _dir) = test_core().await;
        core.store().create_round("autumn", 5).unwrap();
        let user = core.store().account_for_identity("telegram", "222").unwrap();
        core.store().log_request(user, "text").unwrap();

        let SignIn::In { account_id: web } =
            sign_in(&core, "email", "g@example.com").await.unwrap()
        else {
            panic!("the round had room");
        };
        assert!(core.store().is_member(web).unwrap());

        assert_eq!(
            link(&core, web, "telegram", "222").await.unwrap(),
            LinkOutcome::Merged { account_id: user }
        );
        assert!(core.store().is_member(user).unwrap(), "the seat vanished with the account");
        assert_eq!(core.store().rounds().unwrap()[0].used, 1);
    }

    #[tokio::test]
    async fn standing_reads_an_account_without_seating_anyone() {
        let (core, _dir) = test_core().await;
        core.store().create_round("autumn", 1).unwrap();
        let queued = core.store().account_for_identity("email", "waiting@example.com").unwrap();

        // Queued: an account with an identity and no seat.
        let before = standing(&core, queued).await.unwrap();
        assert!(!before.member);
        assert_eq!(before.kinds, vec!["email".to_string()]);

        // And reading it did not take the round's one seat — the failure
        // this is a read rather than a `claim_seat` to avoid.
        let SignIn::In { account_id } = sign_in(&core, "email", "first@example.com").await.unwrap()
        else {
            panic!("the round had a seat and nobody should have spent it");
        };

        let after = standing(&core, account_id).await.unwrap();
        assert!(after.member);

        // Two identities come back sorted, so the page does not shuffle.
        link(&core, account_id, "telegram", "777").await.unwrap();
        assert_eq!(
            standing(&core, account_id).await.unwrap().kinds,
            vec!["email".to_string(), "telegram".to_string()]
        );
    }

    #[tokio::test]
    async fn a_revoked_member_is_not_a_member() {
        // Revocation that a page still reads as membership would be
        // theatre, the same way `claim_seat` treats it.
        let (core, _dir) = test_core().await;
        core.store().create_round("autumn", 5).unwrap();
        let SignIn::In { account_id } = sign_in(&core, "email", "gone@example.com").await.unwrap()
        else {
            panic!("expected In");
        };
        assert!(standing(&core, account_id).await.unwrap().member);

        core.store().revoke(account_id).unwrap();
        assert!(!standing(&core, account_id).await.unwrap().member);
    }

    #[tokio::test]
    async fn a_token_is_valid_once_and_then_already_used() {
        let (core, _dir) = test_core().await;

        issue_token(&core, "hash-1", "reader@example.com", None, 900).await.unwrap();

        let first = consume_token(&core, "hash-1").await.unwrap();
        assert_eq!(
            first,
            TokenOutcome::Valid { email: "reader@example.com".to_string(), account_id: None }
        );

        let second = consume_token(&core, "hash-1").await.unwrap();
        assert_eq!(second, TokenOutcome::AlreadyUsed);
    }
}
