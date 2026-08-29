# Web Identity (W2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a visitor to goodscout.fyi sign in with Telegram or an email magic link, so an open round admits them on the spot and a full one puts them in the queue.

**Architecture:** `scout-core` gains identity lookup, linking and magic-link tokens — all keyed on `account_id`, all ignorant of HTTP. `scout-web` gains sessions (a signed cookie verified without touching the database), Telegram widget verification, a Resend client and the routes. The only behavioural change to existing code is splitting the Telegram delivery write out of `claim_seat`.

**Tech Stack:** Rust, axum 0.8, DuckDB, HMAC-SHA256 (`hmac`/`sha2`), `reqwest` for Resend. No cookie crate and no template engine — the codebase does neither, and this needs about thirty lines of each.

---

## Read this first

**Do not run `cargo fmt`.** This repository is deliberately not rustfmt-formatted.

**Run tests with `--workspace`.** The root manifest is not virtual, so a bare
`cargo test` tests only the root package: `cargo test --workspace`.

**Clippy needs the nix toolchain.** `cargo` is nix 1.96.1 while
`~/.cargo/bin/cargo-clippy` is rustup 1.97, and mixing them fails with
`E0514`. If `cargo clippy` errors that way:

```bash
OUT=$(nix build --no-link --print-out-paths '/nix/store/z75w481isp7d6mbmlif92sabnszzrnmv-clippy-1.96.1.drv^*')
export PATH="$OUT/bin:$PATH"
cargo clippy --workspace --all-targets
```

**Every task must end on a tree that builds and passes.** If a task changes a
function's signature, it updates every caller in the same task.

**Match the house comment style.** Comments explain *why*, especially where a
choice has a cost. Read `crates/scout-core/src/store.rs` around `claim_seat`
for the register.

---

## File structure

| File | Responsibility |
|---|---|
| `crates/scout-core/src/store.rs` *(modify)* | `account_for_identity`, `link_identity`, `LinkOutcome`, login-token rows, migration 6, `claim_seat` split |
| `crates/scout-core/src/identity.rs` *(create)* | the async, `Core`-level API the web calls; no HTTP, no cookies |
| `crates/scout-core/src/invites.rs` *(modify)* | `claim` records the delivery itself |
| `crates/scout-core/src/lib.rs` *(modify)* | `pub mod identity;` |
| `crates/scout-web/src/session.rs` *(create)* | mint/verify the signed cookie; parse and serialise cookie headers |
| `crates/scout-web/src/telegram_login.rs` *(create)* | verify the widget payload; pure function |
| `crates/scout-web/src/ratelimit.rs` *(create)* | in-memory per-key throttle |
| `crates/scout-web/src/email.rs` *(create)* | one Resend HTTP call |
| `crates/scout-web/src/routes/auth.rs` *(create)* | sign-in, magic link, widget callback |
| `crates/scout-web/src/routes/account.rs` *(create)* | `/account`, sign out, link |
| `crates/scout-web/src/pages.rs` *(create)* | the small HTML pages these routes render |
| `crates/scout-web/src/lib.rs` *(modify)* | mount the routes when configured, or not at all |

---

## Task 1: Dependencies for `scout-web`

**Files:** Modify `crates/scout-web/Cargo.toml`

- [ ] **Step 1: Add the dependencies**

```toml
[dependencies]
scout-core = { path = "../scout-core" }
tokio = { version = "1", features = ["macros", "net", "rt-multi-thread", "signal", "time"] }
anyhow = "1"
tracing = "0.1"
axum = "0.8.9"
hmac = "0.12"
sha2 = "0.10"
base64 = "0.22"
rand = "0.8"
chrono = "0.4"
serde = { version = "1", features = ["derive"] }
reqwest = { version = "0.12", features = ["json"] }

[dev-dependencies]
tower = { version = "0.5", features = ["util"] }
```

- [ ] **Step 2: Verify nothing pulled in a second hyper**

Run: `cargo tree -p scout-web 2>/dev/null | grep -c hyper` then
`grep -c 'name = "hyper"' Cargo.lock`

Expected: the `Cargo.lock` count is still `2`. If it became 3, a
dependency dragged in an incompatible hyper — stop and report rather than
proceeding.

- [ ] **Step 3: Build**

Run: `cargo build --workspace`
Expected: success. Unused-dependency warnings are fine at this point.

- [ ] **Step 4: Commit**

```bash
git add crates/scout-web/Cargo.toml Cargo.lock
git commit -m "chore: the dependencies signing in needs"
```

---

## Task 2: `account_for_identity` and `link_identity`

`account_for_telegram` already does lookup-or-create for one fixed kind. This
generalises it so an email identity works the same way, and adds linking.

**Files:** Modify `crates/scout-core/src/store.rs`

- [ ] **Step 1: Write the failing tests**

Add inside `mod tests` in `store.rs`:

```rust
    #[test]
    fn an_identity_is_looked_up_or_created_whatever_its_kind() {
        let (s, _d) = test_store();
        let first = s.account_for_identity("email", "a@example.com").unwrap();
        let again = s.account_for_identity("email", "a@example.com").unwrap();
        assert_eq!(first, again, "the same identity produced two accounts");

        // The generalisation must not have changed what Telegram does.
        let tg = s.account_for_identity("telegram", "11").unwrap();
        assert_eq!(tg, s.account_for_telegram(11).unwrap());
        assert_ne!(tg, first, "two kinds collided into one account");
    }

    #[test]
    fn an_identity_owned_by_someone_else_is_never_moved() {
        let (s, _d) = test_store();
        let owner = s.account_for_identity("telegram", "11").unwrap();
        let other = s.account_for_identity("email", "b@example.com").unwrap();

        assert_eq!(
            s.link_identity(other, "telegram", "11").unwrap(),
            LinkOutcome::TakenByAnother
        );
        // The point of the test: the refusal left ownership alone.
        assert_eq!(s.account_for_identity("telegram", "11").unwrap(), owner);

        assert_eq!(
            s.link_identity(owner, "telegram", "11").unwrap(),
            LinkOutcome::AlreadyYours
        );
        assert_eq!(
            s.link_identity(owner, "email", "c@example.com").unwrap(),
            LinkOutcome::Linked
        );
        assert_eq!(s.account_for_identity("email", "c@example.com").unwrap(), owner);
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p scout-core account_for_identity`
Expected: FAIL — `no method named account_for_identity`.

- [ ] **Step 3: Implement**

Add near `account_for_telegram` (around `store.rs:1092`):

```rust
/// What linking a second way of signing in did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkOutcome {
    Linked,
    AlreadyYours,
    /// Somebody else proved this identity first. Never resolved by moving
    /// it: two sign-ups stay two accounts until a human decides otherwise,
    /// and a wrong merge cannot be undone.
    TakenByAnother,
}
```

```rust
    /// The account proving control of this identity, creating one if the
    /// identity is new.
    ///
    /// `kind` is `&'static str` rather than `&str` on purpose. It is half of
    /// a primary key, and a kind read off the wire — a typo, or a value an
    /// attacker chose — would silently open a parallel identity space that
    /// nothing else can see.
    pub fn account_for_identity(&self, kind: &'static str, external_id: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT account_id FROM identities WHERE kind = ? AND external_id = ?")?;
        let found: Option<i64> =
            stmt.query_map(params![kind, external_id], |r| r.get(0))?.next().transpose()?;
        drop(stmt);
        if let Some(id) = found {
            return Ok(id);
        }
        let account_id: i64 = conn.query_row(
            "INSERT INTO accounts (id) VALUES (nextval('accounts_id_seq')) RETURNING id",
            [],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO identities (account_id, kind, external_id) VALUES (?, ?, ?)",
            params![account_id, kind, external_id],
        )?;
        Ok(account_id)
    }

    /// Attaches a second identity to an account that already exists.
    ///
    /// The `PRIMARY KEY (kind, external_id)` is what actually prevents two
    /// owners under a race; this read exists to produce a sentence a person
    /// can act on instead of a constraint violation.
    pub fn link_identity(
        &self,
        account_id: i64,
        kind: &'static str,
        external_id: &str,
    ) -> Result<LinkOutcome> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT account_id FROM identities WHERE kind = ? AND external_id = ?")?;
        let owner: Option<i64> =
            stmt.query_map(params![kind, external_id], |r| r.get(0))?.next().transpose()?;
        drop(stmt);
        match owner {
            Some(id) if id == account_id => return Ok(LinkOutcome::AlreadyYours),
            Some(_) => return Ok(LinkOutcome::TakenByAnother),
            None => {}
        }
        conn.execute(
            "INSERT INTO identities (account_id, kind, external_id) VALUES (?, ?, ?)",
            params![account_id, kind, external_id],
        )?;
        Ok(LinkOutcome::Linked)
    }
```

Then rewrite `account_for_telegram` to delegate, so there is one
implementation rather than two that can drift:

```rust
    pub fn account_for_telegram(&self, telegram_id: i64) -> Result<i64> {
        self.account_for_identity("telegram", &telegram_id.to_string())
    }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p scout-core`
Expected: PASS, including every existing test — `account_for_telegram` is
used widely and this changed its body.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/store.rs
git commit -m "feat: an identity is an identity, whatever kind it is"
```

---

## Task 3: Migration 6 — `login_tokens`

The schema exists twice: `MIGRATIONS` builds a fresh database in its finished
shape, and `steps()` upgrades an existing one. **Both must gain the table**,
or a fresh database and a migrated one disagree.

**Files:** Modify `crates/scout-core/src/store.rs`

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn a_fresh_database_has_somewhere_to_put_login_tokens() {
        let (s, _d) = test_store();
        assert_eq!(s.schema_version().unwrap(), 6);
        // A fresh database is built by MIGRATIONS and a migrated one by
        // steps(); this fails if only one of the two learned about the table.
        s.issue_login_token("hash-x", "a@example.com", None, 900).unwrap();
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p scout-core login_tokens`
Expected: FAIL — version is 5, and `issue_login_token` does not exist.

- [ ] **Step 3: Add the table to the fresh schema**

In the `MIGRATIONS` string, immediately after the `deliveries` table
(around `store.rs:150`), add:

```sql
-- Magic-link tokens. A row rather than a signed value because a link must
-- be single-use: a replayable one is a standing account key sitting in an
-- inbox. Sign-in is rare, so the store mutex is cheap here in a way it
-- would not be on a per-request session check.
CREATE TABLE IF NOT EXISTS login_tokens (
    token_hash  TEXT PRIMARY KEY,
    email       TEXT NOT NULL,
    -- Set when linking an address to an account that is already signed in;
    -- NULL when the link is a sign-in and the account is not known yet.
    account_id  BIGINT,
    expires_at  TIMESTAMP NOT NULL,
    -- Kept rather than deleted, so "already used" and "expired" stay
    -- distinguishable — they call for different advice.
    consumed_at TIMESTAMP
);
```

- [ ] **Step 4: Add the migration step**

Add the constant near `STEP_5_DELIVERIES` (around `store.rs:585`):

```rust
const STEP_6_LOGIN_TOKENS: &str = r#"
CREATE TABLE IF NOT EXISTS login_tokens (
    token_hash  TEXT PRIMARY KEY,
    email       TEXT NOT NULL,
    account_id  BIGINT,
    expires_at  TIMESTAMP NOT NULL,
    consumed_at TIMESTAMP
);
"#;
```

and extend `steps()`:

```rust
        (5, Step::Sql(STEP_5_DELIVERIES)),
        (6, Step::Sql(STEP_6_LOGIN_TOKENS)),
```

- [ ] **Step 5: Build so the next task compiles**

Run: `cargo build --workspace`
Expected: success. The test still fails — `issue_login_token` arrives in
Task 4 — which is why Steps 6 and 7 are there.

- [ ] **Step 6: Verify the version moved**

Run: `cargo test -p scout-core a_fresh_database_has_somewhere 2>&1 | tail -20`
Expected: still FAIL, but now on `issue_login_token` rather than on the
version assertion.

- [ ] **Step 7: Commit**

```bash
git add crates/scout-core/src/store.rs
git commit -m "feat: schema 6, a place to keep a link that works once"
```

---

## Task 4: Issuing and consuming login tokens

**Files:** Modify `crates/scout-core/src/store.rs`

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn a_token_works_once_and_says_so_afterwards() {
        let (s, _d) = test_store();
        s.issue_login_token("hash-a", "a@example.com", None, 900).unwrap();

        assert_eq!(
            s.consume_login_token("hash-a").unwrap(),
            TokenOutcome::Valid { email: "a@example.com".to_string(), account_id: None }
        );
        // The whole reason the row survives consumption: the second visit
        // gets advice ("you may already be signed in"), not "expired".
        assert_eq!(s.consume_login_token("hash-a").unwrap(), TokenOutcome::AlreadyUsed);
        assert_eq!(s.consume_login_token("hash-never").unwrap(), TokenOutcome::Unknown);
    }

    #[test]
    fn an_expired_token_is_refused_and_not_consumed() {
        let (s, _d) = test_store();
        s.issue_login_token("hash-b", "b@example.com", None, -1).unwrap();
        assert_eq!(s.consume_login_token("hash-b").unwrap(), TokenOutcome::Expired);
        // Expiry must not silently mark it used, or the advice above flips
        // to the wrong branch for anyone who clicks twice.
        assert_eq!(s.consume_login_token("hash-b").unwrap(), TokenOutcome::Expired);
    }

    #[test]
    fn a_link_issued_while_signed_in_remembers_whose_it_is() {
        let (s, _d) = test_store();
        let account = s.account_for_identity("telegram", "11").unwrap();
        s.issue_login_token("hash-c", "c@example.com", Some(account), 900).unwrap();
        assert_eq!(
            s.consume_login_token("hash-c").unwrap(),
            TokenOutcome::Valid { email: "c@example.com".to_string(), account_id: Some(account) }
        );
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p scout-core login_token`
Expected: FAIL — `TokenOutcome` and the methods do not exist.

- [ ] **Step 3: Implement**

Add to `store.rs`, next to the other public types:

```rust
/// What a magic link turned out to be worth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenOutcome {
    Valid { email: String, account_id: Option<i64> },
    Expired,
    AlreadyUsed,
    Unknown,
}
```

and as `Store` methods:

```rust
    /// Records a token that has been mailed out.
    ///
    /// `token_hash` is a hash of the value in the link, never the value:
    /// a database that leaks must not hand over working sign-in links.
    /// `ttl_secs` is signed so a test can issue one already expired.
    pub fn issue_login_token(
        &self,
        token_hash: &str,
        email: &str,
        account_id: Option<i64>,
        ttl_secs: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO login_tokens (token_hash, email, account_id, expires_at)
             VALUES (?, ?, ?, now() + to_seconds(CAST(? AS INTEGER)))",
            params![token_hash, email, account_id, ttl_secs],
        )?;
        Ok(())
    }

    /// Spends a token, if it has anything left to spend.
    ///
    /// Marking consumed and reading the row happen under one mutex, so two
    /// simultaneous clicks cannot both come back `Valid`.
    pub fn consume_login_token(&self, token_hash: &str) -> Result<TokenOutcome> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT email, account_id, consumed_at IS NOT NULL, expires_at < now()
             FROM login_tokens WHERE token_hash = ?",
        )?;
        let row: Option<(String, Option<i64>, bool, bool)> = stmt
            .query_map(params![token_hash], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .next()
            .transpose()?;
        drop(stmt);

        let (email, account_id, used, expired) = match row {
            Some(r) => r,
            None => return Ok(TokenOutcome::Unknown),
        };
        if used {
            return Ok(TokenOutcome::AlreadyUsed);
        }
        if expired {
            return Ok(TokenOutcome::Expired);
        }
        conn.execute(
            "UPDATE login_tokens SET consumed_at = now() WHERE token_hash = ?",
            params![token_hash],
        )?;
        Ok(TokenOutcome::Valid { email, account_id })
    }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p scout-core login_token`
Expected: PASS, including `a_fresh_database_has_somewhere_to_put_login_tokens`
from Task 3.

If `to_seconds` is rejected by this DuckDB build, use
`now() + INTERVAL (CAST(? AS INTEGER)) SECOND` and re-run. Confirm whichever
form works with:
`cargo test -p scout-core a_token_works_once -- --nocapture`

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/store.rs
git commit -m "feat: a sign-in link that is spent when it is used"
```

---

## Task 5: Split the Telegram delivery out of `claim_seat`

`claim_seat` currently writes `deliveries (account_id, 'telegram', chat_id)`
on its waitlist path, from a `chat_id` parameter a web visitor does not have.

**This changes behaviour on purpose, and the test records it:** the address is
now recorded whatever the claim decided, not only when the round was full.
Previously an admitted user had no address until their first message, which
left a real gap — an announcement could not reach someone who was admitted and
had not yet spoken.

**Files:** Modify `crates/scout-core/src/store.rs`, `crates/scout-core/src/invites.rs`

- [ ] **Step 1: Write the failing test**

In `store.rs` `mod tests`:

```rust
    #[test]
    fn claiming_a_seat_records_no_address_of_its_own() {
        let (s, _d) = test_store();
        s.create_round("autumn", 5).unwrap();
        let account = s.account_for_identity("email", "web@example.com").unwrap();

        assert_eq!(s.claim_seat(account, "autumn").unwrap(), Claim::Admitted);

        // The point of the split: a web visitor has no chat, and claiming a
        // seat must not invent one. If this fails, the delivery write has
        // crept back into claim_seat and the web path writes a lie.
        let conn = s.conn.lock().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM deliveries WHERE account_id = ?",
                params![account],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "claim_seat wrote a delivery row");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p scout-core claiming_a_seat_records_no_address`
Expected: FAIL — `claim_seat` takes three arguments.

- [ ] **Step 3: Change the signature and drop the insert**

In `store.rs`, change `pub fn claim_seat(&self, account_id: i64, chat_id: i64, code: &str)`
to `pub fn claim_seat(&self, account_id: i64, code: &str)`, and delete the
`INSERT INTO deliveries …` statement and its comment from the no-room branch
(around `store.rs:1211-1218`). Leave the waitlist insert exactly as it is.

- [ ] **Step 4: Move the delivery write to the Telegram caller**

In `crates/scout-core/src/invites.rs`, replace the body of `claim`:

```rust
pub async fn claim(
    core: &Core,
    telegram_id: i64,
    chat_id: i64,
    code: &str,
) -> anyhow::Result<crate::store::Claim> {
    let store = core.store();
    let code = code.to_string();
    crate::core::blocking(move || {
        let account_id = store.account_for_telegram(telegram_id)?;
        let outcome = store.claim_seat(account_id, &code)?;
        // Where to reach them is the channel's business, not the seat's.
        // Recorded whatever the outcome was: the START they just pressed is
        // the permission, and an admitted person who has not spoken yet
        // still needs to be reachable by an announcement.
        store.note_delivery(account_id, "telegram", &chat_id.to_string())?;
        Ok(outcome)
    })
    .await
}
```

- [ ] **Step 5: Fix every other caller**

Run: `grep -rn "claim_seat" crates/`
Update each remaining call to drop the `chat_id` argument. Existing tests in
`store.rs` that assert a waitlisted person's address reached `deliveries` must
now call `note_delivery` themselves — they are testing the migration and the
announcement path, not `claim_seat`.

- [ ] **Step 6: Run the whole suite**

Run: `cargo test --workspace`
Expected: PASS, 468 or more.

- [ ] **Step 7: Commit**

```bash
git add crates/scout-core/src/store.rs crates/scout-core/src/invites.rs
git commit -m "refactor: a seat is claimed by an account, not by a chat"
```

---

## Task 6: `scout-core/src/identity.rs`

The async surface the web calls. Nothing here knows about HTTP.

**Files:** Create `crates/scout-core/src/identity.rs`; modify `crates/scout-core/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create the file with tests first:

```rust
//! Who an account is, and how a second way of proving it gets attached.
//!
//! Everything here is keyed on `account_id` and knows nothing about
//! cookies, requests or email delivery. That is what lets W4 turn these
//! into API calls without changing their shape.

use crate::core::{blocking, Core};
use crate::store::{Claim, LinkOutcome, TokenOutcome};

/// What signing in produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignIn {
    /// Admitted to a round just now, or already a member.
    In { account_id: i64 },
    /// No room, so queued.
    Queued { account_id: i64 },
}

/// Signs in the account owning this identity, creating it if new, and
/// settles their standing against the newest round with room.
pub async fn sign_in(core: &Core, kind: &'static str, external_id: &str) -> anyhow::Result<SignIn> {
    let store = core.store();
    let external_id = external_id.to_string();
    blocking(move || {
        let account_id = store.account_for_identity(kind, &external_id)?;
        let round = store.rounds()?.into_iter().rfind(|r| r.open && r.used < r.capacity);
        let outcome = match round {
            Some(r) => store.claim_seat(account_id, &r.code)?,
            // No open round at all: claim_seat needs a code to file the
            // waitlist under, so there is nothing to claim against. They
            // are simply not in.
            None => Claim::NoRoom,
        };
        Ok(match outcome {
            Claim::Admitted | Claim::AlreadyIn => SignIn::In { account_id },
            Claim::NoRoom | Claim::Revoked => SignIn::Queued { account_id },
        })
    })
    .await
}

/// Attaches an identity to an account that is already signed in.
pub async fn link(
    core: &Core,
    account_id: i64,
    kind: &'static str,
    external_id: &str,
) -> anyhow::Result<LinkOutcome> {
    let store = core.store();
    let external_id = external_id.to_string();
    blocking(move || store.link_identity(account_id, kind, &external_id)).await
}

/// Records a mailed token.
pub async fn issue_token(
    core: &Core,
    token_hash: &str,
    email: &str,
    account_id: Option<i64>,
    ttl_secs: i64,
) -> anyhow::Result<()> {
    let store = core.store();
    let (token_hash, email) = (token_hash.to_string(), email.to_string());
    blocking(move || store.issue_login_token(&token_hash, &email, account_id, ttl_secs)).await
}

/// Spends a mailed token.
pub async fn consume_token(core: &Core, token_hash: &str) -> anyhow::Result<TokenOutcome> {
    let store = core.store();
    let token_hash = token_hash.to_string();
    blocking(move || store.consume_login_token(&token_hash)).await
}
```

- [ ] **Step 2: Export it**

In `crates/scout-core/src/lib.rs`, add alongside the other `pub mod` lines:

```rust
pub mod identity;
```

`Claim`, `LinkOutcome` and `TokenOutcome` are in the private `store` module,
so re-export them from `identity.rs` so `scout-web` can name them:

```rust
pub use crate::store::{Claim, LinkOutcome, TokenOutcome};
```

- [ ] **Step 3: Check `rounds()` exists with these fields**

Run: `grep -n -A10 "pub fn rounds" crates/scout-core/src/store.rs`
Expected: a `RoundStatus` with `code`, `open`, `capacity`, `used`. If the
field names differ, use the real ones — `crates/scout-core/src/core.rs`
`admission()` uses the same query and is the reference.

- [ ] **Step 4: Build and test**

Run: `cargo build --workspace && cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/identity.rs crates/scout-core/src/lib.rs
git commit -m "feat: core can say who you are without being told how you asked"
```

---

## Task 7: The signed session cookie

Pure computation — no database, no async, no HTTP types.

**Files:** Create `crates/scout-web/src/session.rs`

- [ ] **Step 1: Write the failing tests**

```rust
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
    fn a_cookie_header_yields_only_the_named_cookie() {
        let header = "other=1; scout_session=abc; another=2";
        assert_eq!(read_cookie(header, "scout_session"), Some("abc".to_string()));
        assert_eq!(read_cookie(header, "absent"), None);
        // A prefix must not match: `xscout_session` is not `scout_session`.
        assert_eq!(read_cookie("xscout_session=abc", "scout_session"), None);
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p scout-web session`
Expected: FAIL — the module does not exist.

- [ ] **Step 3: Implement**

Above those tests in the same file:

```rust
//! Sessions as a signed cookie, verified without touching the database.
//!
//! The alternative — a sessions table — would put the store mutex back on
//! a path a stranger can hit, which is exactly the lever the round-state
//! cache exists to remove. The cost of this choice is that a session
//! cannot be revoked: it expires, and rotating the key signs everyone out
//! at once. That is recorded in the design and is not an oversight.

use base64::Engine;
use hmac::{Hmac, Mac};
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

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
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

/// A `Set-Cookie` value that removes the session.
pub fn clear_cookie() -> String {
    format!("{COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0")
}
```

Register it in `crates/scout-web/src/lib.rs`: `mod session;`

- [ ] **Step 4: Run the tests**

Run: `cargo test -p scout-web session`
Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-web/src/session.rs crates/scout-web/src/lib.rs
git commit -m "feat: a session you can verify without asking the database"
```

---

## Task 8: Telegram Login Widget verification

**Files:** Create `crates/scout-web/src/telegram_login.rs`

Telegram signs the widget payload with `HMAC-SHA256(data_check_string,
SHA256(bot_token))`, where `data_check_string` is every field except `hash`,
as `key=value`, sorted by key, joined by `\n`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "123456:test-bot-token";

    /// Builds a payload signed the way Telegram signs one, so the test
    /// exercises the real algorithm rather than our own idea of it.
    fn signed(fields: &[(&str, &str)]) -> Vec<(String, String)> {
        use hmac::{Hmac, Mac};
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
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p scout-web telegram_login`
Expected: FAIL — module missing.

- [ ] **Step 3: Implement**

```rust
//! Verifying the Telegram Login Widget's payload.
//!
//! This lives in `scout-web` rather than `scout-telegram` because it is
//! arithmetic, not Telegram integration: HMAC over sorted fields with
//! `SHA256(bot_token)` as the key. Keeping it here means `scout-web` needs
//! no dependency on the Telegram adapter, which the design requires, and
//! it is testable against a fixture with no network.

use hmac::{Hmac, Mac};
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
```

Make `constant_time_eq` visible to this module: in `session.rs` change
`fn constant_time_eq` to `pub(crate) fn constant_time_eq`.

Register in `lib.rs`: `mod telegram_login;`

- [ ] **Step 4: Run the tests**

Run: `cargo test -p scout-web telegram_login`
Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-web/src/telegram_login.rs crates/scout-web/src/session.rs crates/scout-web/src/lib.rs
git commit -m "feat: prove a Telegram login without trusting the browser"
```

---

## Task 9: The rate limiter

**Files:** Create `crates/scout-web/src/ratelimit.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_key_is_allowed_its_quota_and_then_refused() {
        let l = Limiter::new(3, Duration::from_secs(900));
        assert!(l.allow("a@example.com"));
        assert!(l.allow("a@example.com"));
        assert!(l.allow("a@example.com"));
        assert!(!l.allow("a@example.com"), "a fourth request in the window was allowed");
        // One noisy address must not lock out everyone else.
        assert!(l.allow("b@example.com"));
    }

    #[test]
    fn the_window_lets_go() {
        let l = Limiter::new(1, Duration::from_millis(50));
        assert!(l.allow("k"));
        assert!(!l.allow("k"));
        std::thread::sleep(Duration::from_millis(60));
        assert!(l.allow("k"), "the window never expired");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p scout-web ratelimit`
Expected: FAIL — module missing.

- [ ] **Step 3: Implement**

```rust
//! A per-key request throttle held in memory.
//!
//! In memory rather than in the database for the same reason sessions are:
//! this is consulted on a path a stranger controls, and the database must
//! not be. The worst case of the choice is that a deploy forgets who was
//! being noisy, which is the right shape of failure for a counter.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct Limiter {
    quota: usize,
    window: Duration,
    seen: Mutex<HashMap<String, Vec<Instant>>>,
}

impl Limiter {
    pub fn new(quota: usize, window: Duration) -> Self {
        Self { quota, window, seen: Mutex::new(HashMap::new()) }
    }

    /// True when this key may proceed, counting the attempt.
    pub fn allow(&self, key: &str) -> bool {
        let now = Instant::now();
        // A poisoned lock means another thread panicked mid-update. The
        // stake is a rate counter, so recovering and carrying on beats
        // taking the sign-in page down.
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());

        // Sweep every key, not just this one: otherwise an attacker who
        // never repeats an address grows the map without bound.
        seen.retain(|_, hits| {
            hits.retain(|t| now.duration_since(*t) < self.window);
            !hits.is_empty()
        });

        let hits = seen.entry(key.to_string()).or_default();
        if hits.len() >= self.quota {
            return false;
        }
        hits.push(now);
        true
    }
}
```

Register in `lib.rs`: `mod ratelimit;`

- [ ] **Step 4: Run the tests**

Run: `cargo test -p scout-web ratelimit`
Expected: PASS, 2 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-web/src/ratelimit.rs crates/scout-web/src/lib.rs
git commit -m "feat: one address cannot make Scout send mail all day"
```

---

## Task 10: The Resend client

**Files:** Create `crates/scout-web/src/email.rs`

- [ ] **Step 1: Write the failing test**

Only the message body is testable without a network; the HTTP call is
exercised by hand in Task 15.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_carries_the_link_and_says_what_it_is_for() {
        let body = body("https://goodscout.fyi/auth/email?t=abc");
        assert!(body.contains("https://goodscout.fyi/auth/email?t=abc"));
        // Someone who did not ask for this must be told what happened
        // rather than left to guess.
        assert!(body.to_lowercase().contains("did not"));
        assert!(body.contains("15 minutes"), "the expiry is not stated");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p scout-web email`
Expected: FAIL — module missing.

- [ ] **Step 3: Implement**

```rust
//! Sending the sign-in link, through Resend.

/// The plain-text message. Separate from sending so it can be asserted on
/// without a network.
pub fn body(link: &str) -> String {
    format!(
        "Sign in to Scout by opening this link:\n\n\
         {link}\n\n\
         It works once and expires in 15 minutes.\n\n\
         If you did not ask to sign in, ignore this — nothing has happened \
         to any account.\n"
    )
}

/// Sends the link. `Err` when Resend would not take it, so the caller can
/// tell the visitor the truth rather than claim success into a void.
pub async fn send(api_key: &str, from: &str, to: &str, link: &str) -> anyhow::Result<()> {
    let res = reqwest::Client::new()
        .post("https://api.resend.com/emails")
        .bearer_auth(api_key)
        .json(&serde_json::json!({
            "from": from,
            "to": [to],
            "subject": "Sign in to Scout",
            "text": body(link),
        }))
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status();
        let detail = res.text().await.unwrap_or_default();
        anyhow::bail!("resend refused the message: {status} {detail}");
    }
    Ok(())
}
```

Add `serde_json = "1"` to `crates/scout-web/Cargo.toml` dependencies, and
register in `lib.rs`: `mod email;`

- [ ] **Step 4: Run the tests**

Run: `cargo test -p scout-web email`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-web/src/email.rs crates/scout-web/Cargo.toml Cargo.lock crates/scout-web/src/lib.rs
git commit -m "feat: mail a link that says what it is and when it dies"
```

---

## Task 11: Configuration, and mounting nothing when unconfigured

**Files:** Modify `crates/scout-web/src/lib.rs`

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn without_a_session_key_the_auth_routes_do_not_exist() {
        let cache = crate::cache::AdmissionCache::new(scout_core::core::Admission::Full);
        // Unconfigured: no key, so nothing that mints a session is served.
        // Booting with a generated default would sign sessions that a
        // restart could not verify, and nobody would notice until someone
        // forged one.
        let app = router(cache, None);
        let res = app
            .oneshot(Request::builder().uri("/sign-in").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p scout-web without_a_session_key`
Expected: FAIL — `router` takes one argument.

- [ ] **Step 3: Implement**

Add to `lib.rs`:

```rust
/// Everything the signed-in half of the site needs. Absent when the
/// deployment has not been given the keys for it.
#[derive(Clone)]
pub struct AuthConfig {
    pub session_key: Vec<u8>,
    pub bot_token: String,
    pub resend_api_key: String,
    pub mail_from: String,
    pub base_url: String,
}

/// The router's state for the signed-in half. The limiters live here, in
/// one place, because they are shared across requests by definition — a
/// per-request limiter counts to one and stops nothing.
#[derive(Clone)]
pub struct AuthState {
    pub cfg: std::sync::Arc<AuthConfig>,
    pub core: std::sync::Arc<scout_core::core::Core>,
    pub by_address: std::sync::Arc<crate::ratelimit::Limiter>,
    pub by_ip: std::sync::Arc<crate::ratelimit::Limiter>,
}

impl AuthState {
    pub fn new(cfg: AuthConfig, core: std::sync::Arc<scout_core::core::Core>) -> Self {
        use std::time::Duration;
        Self {
            cfg: std::sync::Arc::new(cfg),
            core,
            by_address: std::sync::Arc::new(crate::ratelimit::Limiter::new(3, Duration::from_secs(900))),
            by_ip: std::sync::Arc::new(crate::ratelimit::Limiter::new(10, Duration::from_secs(3600))),
        }
    }
}

impl AuthConfig {
    /// Reads the environment, returning `None` unless every key is present.
    ///
    /// All or nothing on purpose: a half-configured deployment that serves
    /// a sign-in form and then cannot mail anything is worse than one that
    /// does not offer sign-in at all.
    pub fn from_env() -> Option<Self> {
        let key = std::env::var("SCOUT_SESSION_KEY").ok()?;
        if key.len() < 32 {
            tracing::warn!("SCOUT_SESSION_KEY is shorter than 32 bytes; sign-in stays off");
            return None;
        }
        Some(Self {
            session_key: key.into_bytes(),
            bot_token: std::env::var("TELEGRAM_BOT_TOKEN").ok()?,
            resend_api_key: std::env::var("RESEND_API_KEY").ok()?,
            mail_from: std::env::var("SCOUT_MAIL_FROM").ok()?,
            base_url: std::env::var("SCOUT_BASE_URL").ok()?,
        })
    }
}
```

Change `router` to take `Option<AuthState>` and merge the auth routes only
when it is `Some`. `serve` calls `AuthConfig::from_env()` and logs which way
it went, so an operator can see it in the startup log rather than by probing:

```rust
    let auth = AuthConfig::from_env();
    match &auth {
        Some(_) => tracing::info!("sign-in is configured"),
        None => tracing::info!("sign-in is not configured; serving the public page only"),
    }
```

Confirm the bot-token variable's real name first:
Run: `grep -rn "TELEGRAM_BOT_TOKEN\|bot_token" crates/scout-core/src/config.rs .env.example`
and use whatever `.env.example` actually calls it.

- [ ] **Step 4: Run the tests**

Run: `cargo test --workspace`
Expected: PASS. Existing `router(cache)` calls in tests need `None` added.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-web/src/lib.rs
git commit -m "feat: sign-in exists only where it has been configured"
```

---

## Task 12: Sign-in by email — request and confirm

**Files:** Create `crates/scout-web/src/pages.rs`, `crates/scout-web/src/routes/auth.rs`

- [ ] **Step 1: Write the failing tests**

```rust
    #[tokio::test]
    async fn a_get_on_the_emailed_link_consumes_nothing() {
        // Corporate mail scanners follow links before the human does. If
        // GET consumed the token, the scanner would burn it and the human
        // would be told the link had expired — a failure that appears only
        // for users at exactly the organisations hardest to reproduce.
        let (app, core, _dir) = test_app().await;
        let token = "tok-scanner";
        issue(&core, token).await;

        let res = get(&app, &format!("/auth/email?t={token}")).await;
        assert_eq!(res.status(), StatusCode::OK);

        // Still spendable afterwards.
        assert!(matches!(
            scout_core::identity::consume_token(&core, &hash(token)).await.unwrap(),
            scout_core::identity::TokenOutcome::Valid { .. }
        ));
    }

    #[tokio::test]
    async fn requesting_a_link_says_the_same_thing_for_any_address() {
        // Answering differently for a known address turns the form into a
        // membership oracle.
        let (app, _core, _dir) = test_app().await;
        let known = post_form(&app, "/sign-in/email", "email=known%40example.com").await;
        let unknown = post_form(&app, "/sign-in/email", "email=nobody%40example.com").await;
        assert_eq!(known.status(), unknown.status());
        assert_eq!(body_of(known).await, body_of(unknown).await);
    }
```

These need a harness. Add `tempfile = "3"` to `scout-web`'s
`[dev-dependencies]` and write it once, at the top of `mod tests`, so every
later task can use it:

```rust
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::response::Response;
    use tower::ServiceExt;

    pub(crate) const TEST_KEY: &[u8] = b"a test session key of at least 32 bytes";

    /// A router over a real Core on a throwaway database.
    ///
    /// The TempDir is returned and must be held: dropping it deletes the
    /// database out from under the still-open connection.
    pub(crate) async fn test_app()
        -> (axum::Router, std::sync::Arc<scout_core::core::Core>, tempfile::TempDir)
    {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("test.duckdb");
        let cfg = scout_core::config::Config::for_test(db.to_str().unwrap());
        let core = std::sync::Arc::new(scout_core::core::Core::start(cfg, None).unwrap());

        let auth = crate::AuthConfig {
            session_key: TEST_KEY.to_vec(),
            bot_token: "123456:test-bot-token".to_string(),
            resend_api_key: "test-key".to_string(),
            mail_from: "Scout <hello@example.com>".to_string(),
            base_url: "https://example.com".to_string(),
        };
        let cache = crate::cache::AdmissionCache::new(scout_core::core::Admission::Full);
        let app = crate::router(cache, Some(crate::AuthState::new(auth, core.clone())));
        (app, core, dir)
    }

    pub(crate) fn hash(token: &str) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
    }

    pub(crate) async fn issue(core: &scout_core::core::Core, token: &str) {
        scout_core::identity::issue_token(core, &hash(token), "a@example.com", None, 900)
            .await.unwrap();
    }

    pub(crate) async fn open_round(core: &scout_core::core::Core, code: &str, capacity: i64) {
        scout_core::invites::open(core, code, capacity).await.unwrap();
    }

    pub(crate) async fn get(app: &axum::Router, uri: &str) -> Response {
        app.clone().oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await.unwrap()
    }

    pub(crate) async fn get_with_cookie(app: &axum::Router, uri: &str, cookie: &str) -> Response {
        app.clone().oneshot(
            Request::builder().uri(uri)
                .header("cookie", format!("{}={cookie}", crate::session::COOKIE))
                .body(Body::empty()).unwrap()
        ).await.unwrap()
    }

    pub(crate) async fn post_form(app: &axum::Router, uri: &str, form: &str) -> Response {
        app.clone().oneshot(
            Request::builder().method("POST").uri(uri)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(form.to_string())).unwrap()
        ).await.unwrap()
    }

    pub(crate) async fn post_with_cookie(app: &axum::Router, uri: &str, cookie: &str) -> Response {
        app.clone().oneshot(
            Request::builder().method("POST").uri(uri)
                .header("cookie", format!("{}={cookie}", crate::session::COOKIE))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::empty()).unwrap()
        ).await.unwrap()
    }

    pub(crate) async fn body_of(res: Response) -> String {
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    }
```

**`open_round` assumes `scout_core::invites::open(core, code, capacity)`.**
Confirm the real name first — `grep -n "pub async fn" crates/scout-core/src/invites.rs`
— and if the crate only exposes round creation through `InviteCmd`, call
`core.store().create_round(code, capacity)` from a `blocking` closure instead.

Every test that calls `test_app()` binds three values, the third being the
TempDir: `let (app, core, _dir) = test_app().await;`

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p scout-web auth`
Expected: FAIL — routes missing.

- [ ] **Step 3: Implement the routes**

`POST /sign-in/email`: rate-limit on the address and on the peer IP; generate
32 random bytes, hex-encode as the token; store `SHA-256` of it via
`identity::issue_token(.., ttl_secs = 900)`; mail
`{base_url}/auth/email?t={token}`; **always** render the same "check your
inbox" page, whether or not the address was known and even if the address was
malformed.

`GET /auth/email?t=…`: render a page with a **Confirm sign-in** button that
`POST`s the token to `/auth/email`. Consume nothing. Send
`Referrer-Policy: no-referrer`.

`POST /auth/email`: `identity::consume_token`, then match:
- `Valid { email, account_id: None }` → `identity::sign_in(core, "email", &email)`, set the cookie, redirect to `/account`
- `Valid { email, account_id: Some(id) }` → `identity::link(core, id, "email", &email)`, redirect to `/account`
- `Expired` → "that link has expired", with a link back to `/sign-in`
- `AlreadyUsed` → "that link has already been used — you may already be signed in", with a link to `/account`
- `Unknown` → same page as `Expired`; distinguishing them would confirm which tokens have existed

- [ ] **Step 4: Run the tests**

Run: `cargo test -p scout-web`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-web/src/routes crates/scout-web/src/pages.rs crates/scout-web/Cargo.toml
git commit -m "feat: sign in by a link that a mail scanner cannot spend"
```

---

## Task 13: Sign-in by Telegram, `/account`, sign out, link

**Files:** Create `crates/scout-web/src/routes/account.rs`; modify `crates/scout-web/src/routes/auth.rs`

- [ ] **Step 1: Write the failing tests**

```rust
    #[tokio::test]
    async fn signing_in_at_an_open_round_admits_and_a_full_one_queues() {
        let (app, core, _dir) = test_app().await;
        open_round(&core, "autumn", 1).await;

        let first = scout_core::identity::sign_in(&core, "email", "a@example.com").await.unwrap();
        assert!(matches!(first, scout_core::identity::SignIn::In { .. }));

        // Capacity is one, so the next person queues rather than being
        // turned away with nothing — the dead end W1 left, closed.
        let second = scout_core::identity::sign_in(&core, "email", "b@example.com").await.unwrap();
        assert!(matches!(second, scout_core::identity::SignIn::Queued { .. }));
    }

    #[tokio::test]
    async fn account_needs_a_session_and_sign_out_ends_it() {
        let (app, _core, _dir) = test_app().await;
        assert_eq!(get(&app, "/account").await.status(), StatusCode::SEE_OTHER);

        let cookie = crate::session::mint(TEST_KEY, 1, 3600);
        let signed_in = get_with_cookie(&app, "/account", &cookie).await;
        assert_eq!(signed_in.status(), StatusCode::OK);

        let out = post_with_cookie(&app, "/sign-out", &cookie).await;
        let set = out.headers()["set-cookie"].to_str().unwrap();
        assert!(set.contains("Max-Age=0"), "sign out did not clear the cookie");
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p scout-web account`
Expected: FAIL.

- [ ] **Step 3: Implement**

`GET /auth/telegram`: collect the query pairs, `telegram_login::verify(&bot_token, &pairs)`.
- `None` → 400, no detail about which check failed
- `Some(id)` and no session → `identity::sign_in(core, "telegram", &id.to_string())`, set cookie, redirect to `/account`
- `Some(id)` with a session → `identity::link(core, account_id, "telegram", &id.to_string())`, redirect to `/account` with the outcome shown

`GET /account`: no valid cookie → `303` to `/sign-in`. Otherwise render standing
(member, or queued), the linked identities, a control to link whichever method
is missing, and sign out. When they are a member, show the Telegram deep link —
it is how Scout is used until W3.

`POST /sign-out`: reply with `session::clear_cookie()` and `303` to `/`.

CSRF: every `POST` carries a hidden field whose value is
`session::mint(key, account_id, 900)` and is checked with `session::verify`
before acting. `SameSite=Lax` does not cover cross-site `POST`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-web/src/routes
git commit -m "feat: an account page, and two ways to reach it"
```

---

## Task 14: Security headers and the widget's origin

**Files:** Modify `crates/scout-web/src/lib.rs`; modify the Caddy/ingress config

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn the_sign_in_page_allows_telegram_and_nothing_else() {
        let (app, _core, _dir) = test_app().await;
        let res = get(&app, "/sign-in").await;
        let csp = res.headers()["content-security-policy"].to_str().unwrap();
        assert!(csp.contains("https://telegram.org"));
        // The widget is the only external resource this site has ever
        // needed. If a second origin appears here, someone should have to
        // change this line and explain why.
        assert_eq!(csp.matches("https://").count(), 1);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p scout-web the_sign_in_page_allows_telegram`
Expected: FAIL — no CSP header.

- [ ] **Step 3: Implement**

Emit on the sign-in page:

```
Content-Security-Policy: default-src 'self'; script-src 'self' https://telegram.org; frame-src https://telegram.org; img-src 'self' data:; style-src 'self' 'unsafe-inline'
Referrer-Policy: no-referrer
X-Content-Type-Options: nosniff
X-Frame-Options: DENY
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --workspace` and then clippy per "Read this first".
Expected: PASS, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-web/src
git commit -m "feat: one external origin, named and asserted"
```

---

## Task 15: Configuration, deploy, and a real end-to-end sign-in

**Files:** Modify `.env.example`, `README.md`

- [ ] **Step 1: Document the new keys**

Add to `.env.example`:

```bash
# Web sign-in (W2). All four must be set or sign-in stays off entirely.
# SCOUT_SESSION_KEY signs session cookies: 32+ bytes, `openssl rand -base64 32`.
# Changing it signs everyone out, which is also how you revoke a session.
SCOUT_SESSION_KEY=
RESEND_API_KEY=
SCOUT_MAIL_FROM=Scout <hello@goodscout.fyi>
SCOUT_BASE_URL=https://goodscout.fyi
```

- [ ] **Step 2: Confirm they reach the pod and not the backup secret**

Run: `grep -vE '^(AWS_|RESTIC_)' .env | grep -cE '^(SCOUT_SESSION_KEY|RESEND_API_KEY|SCOUT_MAIL_FROM|SCOUT_BASE_URL)='`
Expected: `4` — they belong in the `scout` Secret, not `scout-offsite`.

- [ ] **Step 3: Deploy**

```bash
export SCOUT_SSH=root@169.58.231.116
export SCOUT_DOMAIN=$(grep '^SCOUT_DOMAIN=' .env | cut -d= -f2-)
export SCOUT_ACME_EMAIL=$(grep '^SCOUT_ACME_EMAIL=' .env | cut -d= -f2-)
./scripts/deploy-k3s.sh --dry-run   # read the plan first
./scripts/deploy-k3s.sh
```

- [ ] **Step 4: Verify the migration ran**

Run: `ssh $SCOUT_SSH 'kubectl -n scout logs deploy/scout --tail=100 | grep -i "schema\|migrat"'`
Expected: schema 6. **Confirm a pre-migration backup was written** —
`ls /data/backups/` should hold a `-migration-` file dated today.

- [ ] **Step 5: Sign in for real**

Only now, with DNS verified at Resend, request a link at
`https://goodscout.fyi/sign-in`, confirm it, and land on `/account`.

Then check the three things unit tests cannot:
- the mail arrives and is **not** in spam
- the same link, opened a second time, says "already used"
- signing in with Telegram while signed in by email links rather than replacing

- [ ] **Step 6: Commit**

```bash
git add .env.example README.md
git commit -m "docs: the four keys that turn sign-in on"
```

---

## Before starting

**Resend domain verification for `goodscout.fyi` — SPF, DKIM and DMARC at
Porkbun — must be started before Task 1**, not before Task 15. There is
currently no email DNS at all, verification plus propagation is hours to a
day, and Task 15 is the only place it can be tested. Started now it overlaps
the whole build; left until it blocks, it is a day of nothing.
