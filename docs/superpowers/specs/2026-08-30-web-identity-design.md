# Web Identity (W2) — Design

## Purpose

W1 put a public page on the internet that reports whether the current invite
round has room. It has no sign-in, by design, and that leaves a known dead
end: a visitor who arrives at a full round is told it is full and offered
nothing. There is nowhere to leave a name.

This designs the sign-in — the phase that turns a visitor into an account, so
that a full round becomes a queue rather than a closed door, and so that the
web chat in W3 has someone to be talking to.

## Scope

W2 only. Sign-in by Telegram widget or email magic link, a session, linking a
second identity to an existing account, claiming a seat from the web, and
joining the waitlist when there is no seat to claim.

Explicitly **not** the chat, and **not** re-keying core off Telegram
identifiers. Nothing in W2 runs an agent, so `run_agent`'s Telegram-shaped
signature is untouched here. That work is real and it is W3's; see "Deferred".

## Verified before designing

Read out of the code rather than remembered, because three of these change
what the phase costs.

**The schema already anticipates this.** `identities(account_id, kind,
external_id)` carries the comment *"`kind` is 'telegram' today; a web login is
a second kind"*, with `PRIMARY KEY (kind, external_id)`. `deliveries` is
already `(account_id, channel, address)`. `conversations.scope` already
records that `'direct'` is *"the 1:1 chat and the web app, which share"*.
W2 adds one table and no columns.

**The Telegram Login Widget needs no new identity kind.** It returns a
Telegram user id, which is `kind='telegram'` — the identity an existing user
already has. So signing in with Telegram maps a visitor onto the account
holding their purchase history, with no linking step at all. Only email is a
new kind.

**`claim_seat` is coupled to Telegram and must be split.** It writes
`INSERT INTO deliveries (account_id, channel, address) VALUES (?, 'telegram',
?)` as part of claiming, from a `chat_id` parameter. A web visitor has no
chat_id at that moment. See "The one refactor in core".

**The streaming half of W3 already exists.** `scout_api::AgentEvent` is
`Serialize`/`Deserialize` with `Tool`/`Answer`/`Thinking`/`Notice`, and
`run_agent` already emits into an `EventSink`. Noted here so W2 does not
build a transport nobody asked it for.

**No email DNS exists.** `goodscout.fyi` has no SPF, no DKIM on any common
selector, no DMARC, no MX. W1's design named these as the one thing worth
doing early *because they cost wall-clock rather than work*, and it did not
happen. It is now the long pole.

## Decisions taken before designing

**Sessions are a signed cookie, verified without touching the database.**
W1 built a round-state cache specifically so that a public request never takes
the store mutex — a lever a stranger could pull to make Scout slow for
everyone, no exploit required, only traffic. Authenticating against a
`sessions` table would put that lever back, on a path a stranger can hit. So
the cookie carries `account_id`, an expiry and a nonce, HMAC-signed;
verification is pure computation.

This keeps the lever off *session verification*, and nothing more: sign-in
itself still writes to the database from an unauthenticated route. See the
security posture below, which says what that costs and what is and is not
done about it.

The cost is real and accepted: **there is no per-session revocation.**
Sessions expire rather than being killed, and rotating the key signs everyone
out at once. For a product whose entire userbase currently fits in one invite
round, that is the right trade. The escape hatch, if it ever matters, is an
in-memory revocation set — which is why nothing here depends on sessions being
unrevocable.

**Signing in at an open round claims a seat.** Not "identity only, admission
still happens in Telegram". A visitor who signs in while there is room becomes
a member immediately, and the page then hands them the Telegram deep link as
today's way to actually use Scout. One admission path, and the account is
ready for web chat the day W3 lands. If the round is full they join the
waitlist, which is the dead end closing.

**Email goes through Resend.** 3,000 messages a month free covers a waitlist
many times over, it is one REST call from Rust with no SDK, and its DNS
verification is minutes rather than the day Amazon SES spends in sandbox
review. Postmark has better deliverability and was the alternative; the
volume here does not justify its bill, and a magic link is mail the recipient
asked for seconds ago and is actively watching for, which is the easiest
deliverability case there is.

**Linking never merges accounts.** Authenticating a second way *while signed
in* attaches that identity to the current account. If the identity already
belongs to a different account, the attempt is refused with an explanation.
Two separate sign-ups stay two accounts until someone deliberately joins them,
because the alternative can silently merge two people, and no amount of care
afterwards unmerges purchase histories.

## Architecture

No new crate. The split follows the boundary the workspace phase drew: core
owns the database and the account model, adapters own their transport.

| Where | Responsibility |
|---|---|
| `scout-core/src/identity.rs` *(new, pub)* | account-for-identity, link-identity, claim-seat-by-account, join-waitlist-by-account — all keyed on `account_id` |
| `scout-core/src/store.rs` | `account_for_identity`, `link_identity`, `claim_seat` split, migration 6 |
| `scout-web/src/session.rs` *(new)* | mint and verify the signed cookie; pure, no database |
| `scout-web/src/telegram_login.rs` *(new)* | verify the widget payload; a pure function |
| `scout-web/src/email.rs` *(new)* | one Resend HTTP call |
| `scout-web/src/routes/auth.rs` *(new)* | routes, cookies, CSRF |
| `scout-web/src/account.html` *(new)* | the signed-in page |

**Core never learns what a cookie is.** It gains no notion of sessions, HTTP,
or email. It answers "which account owns this identity" and "attach this
identity to that account". This is what keeps W4 possible: those calls become
API calls without changing shape.

**Widget verification lives in `scout-web`, not `scout-telegram`.** It is
HMAC-SHA256 over the payload's sorted fields with `SHA256(bot_token)` as the
key — arithmetic, not Telegram integration. Putting it in `scout-web` as a
pure function keeps `scout-web` free of any dependency on `scout-telegram`,
which the W1 design requires, and makes it testable against a fixture with no
network.

## Sessions

Cookie `__Host-scout_session`: `account_id`, expiry, nonce, HMAC-SHA256 over
all three. `HttpOnly`, `Secure`, `SameSite=Lax`, `Path=/`, no `Domain`,
30 days.

The `__Host-` prefix is what stops a neighbouring host writing this name. It
was added after the W2 review; the cookie shipped without it, so the rename
signed out everyone holding an old one — one person, who signed in again.
The prefix's three conditions are not optional decoration: a browser that
sees `__Host-` without `Secure`, without `Path=/`, or with a `Domain` refuses
to store the cookie at all.

`Lax` rather than `Strict` is deliberate: the Telegram widget returns through
a cross-site navigation, and `Strict` would drop the cookie at exactly that
moment. `Lax` still withholds the cookie from cross-site `POST`, which is why
CSRF protection below is separate rather than assumed.

The signing key is `SCOUT_SESSION_KEY`. **If it is unset the auth routes do
not mount**, and the site serves exactly what it serves today. Failing closed
and visibly beats booting with a generated or default key, which would sign
sessions that a second replica — or a restart — could not verify, and which
nobody would notice until someone forged one.

## Magic links

Tokens get a real table, `login_tokens`, and this is not a reversal of the
stateless-session decision. **A session is verified on every request; a token
is consumed once, at sign-in.** Sign-in is rare, so taking the store mutex
there costs nothing that matters, and single-use is worth a row — a
replayable link is a standing account key sitting in an inbox.

```sql
CREATE TABLE login_tokens (
    token_hash TEXT PRIMARY KEY,   -- SHA-256 of the token in the link
    email      TEXT NOT NULL,
    account_id BIGINT,             -- set when linking while signed in; NULL when signing in
    expires_at TIMESTAMP NOT NULL,
    consumed_at TIMESTAMP
);
```

Stored as a hash, never raw, so the database does not hold a working
credential. **Valid for 15 minutes** — long enough to switch to a mail client
and back, short enough that a link left in an inbox stops being a key by the
time anyone finds it. Consumed rows are kept, not deleted, so "already used"
can be distinguished from "expired"; a row older than a day is swept by the
existing maintenance loop.

**Corporate mail scanners follow links before the human does.** A `GET` that
consumes the token means the scanner burns it and the recipient is told the
link has expired — a failure that appears only for users at exactly the
organisations most likely to have such scanning, and that is nearly
impossible to reproduce locally. So the emailed link is a `GET` that renders
a **Confirm sign-in** button and consumes nothing; the `POST` behind it
consumes. A preview costs nothing.

`Referrer-Policy: no-referrer` on that page keeps the token out of `Referer`
headers on any link the page might carry.

## The one refactor in core

`claim_seat(account_id, chat_id, code)` claims the seat and writes the
Telegram delivery row together. Split:

```rust
claim_seat(account_id, code)                    // members / waitlist only
record_delivery(account_id, channel, address)   // the Telegram adapter, as today
```

Behaviour is unchanged for Telegram — the adapter calls both. The web calls
only the first, because a web visitor has no address until they open Telegram
through the deep link, at which point the existing code records it.

This is a down payment on W3's `(channel, address)` work rather than a
workaround for it: the coupling being removed is precisely the coupling that
makes `run_agent` Telegram-shaped.

## Routes

```
GET  /                     landing page, now aware of session
GET  /sign-in              choose: Telegram widget, or an email field
POST /sign-in/email        request a link                    ← rate limited
GET  /auth/email?t=…       renders "Confirm sign-in"; consumes nothing
POST /auth/email           consumes the token, sets the cookie
GET  /auth/telegram        widget callback: verify, set the cookie
GET  /account              status, linked identities, sign out
POST /account/link/email   request a link while signed in
POST /sign-out             clear the cookie
GET  /healthz              unchanged
GET  /icon.svg             unchanged
```

## What happens on sign-in

1. Authenticate to an `account_id` — found by identity, or created with it
2. Already a member → `/account` says so
3. A round has room → `claim_seat` → a member, plus the Telegram deep link
4. Otherwise → waitlist → queued

## Security posture

W1's posture was *"no input of any kind… the injection surface is nil"*. W2
ends that, so the posture is restated rather than inherited.

- **Email sign-in is the one place a stranger causes work** — an outbound
  message and a row. Throttled in memory: **3 requests per address per 15
  minutes, 10 per IP per hour**. In memory rather than in the database, for
  the same reason sessions are stateless — and a counter whose worst case is
  being reset by a deploy is the right shape for this.

  A limit is only worth what its *key* is worth, which the W2 security
  review found out the hard way. Both keys are normalised before counting:
  an address is lower-cased, its `+tag` stripped, and its dots stripped at
  the providers known to ignore them, so that forty spellings of one inbox
  are one bucket; a client address is keyed by its /64 when it is IPv6,
  since the smallest allocation an IPv6 client gets is a /64 and keying on
  the /128 gave one ordinary client one bucket per request. The forwarded
  address is read from the *last* `X-Forwarded-For` entry — the one our own
  proxy appended — and a request arriving with no usable forwarded address
  falls into a single shared bucket rather than being exempt.
- **Tokens** hashed at rest, 15-minute expiry, single-use, consumed by `POST`.
- **CSRF**: state-changing routes are `POST` carrying a signed double-submit
  token. `SameSite=Lax` does not cover cross-site `POST` and is not treated as
  if it does.
- **Enumeration**: requesting a link answers identically whether or not the
  address is known to us.
- **Widget replay**: Telegram's payload carries `auth_date`; anything older
  than 60 seconds is refused even with a valid HMAC.
- **CSP** gains exactly one external origin, `telegram.org`, for the widget
  script — the only resource the site loads from anywhere but itself.
- **The database stays off the per-request path**, as in W1. Reading a page
  does not touch it. Sign-in does — and this is where the claim above, that
  keeping sessions stateless means "a stranger cannot make the site take the
  store mutex", is **not true**. It is true of session *verification*, which
  is what that decision was about. It is false of `POST /sign-in/email`,
  which is unauthenticated, reachable by anybody, and writes a
  `login_tokens` row through the same `Arc<Mutex<Connection>>` the agent
  uses. A flood of sign-in requests degrades the bot for real users, with no
  exploit involved.

  **This is not closed, and closing it needs a queue** — a writer task the
  web layer hands work to, so that a request can be dropped instead of
  waiting on the mutex. That is not built. What *is* done is bounding the
  reachable rate: with the keys above actually binding, the writes a
  stranger can cause are 3 per inbox per 15 minutes and 10 per client per
  hour, and `login_tokens` is pruned a day past expiry in the maintenance
  loop rather than kept forever, so the table is no longer a size a stranger
  chooses either. The residual is that an attacker with many client
  addresses still gets one write each, at 10 an hour apiece.

## Failure handling

**Resend unreachable.** The page says the mail did not go out and offers a
retry, rather than reporting success into a void. The token remains valid.

**Expired or already-consumed token.** Say which, and offer a fresh one.
Distinguishing them is worth it: "expired" means try again, "already used"
means check whether you are already signed in somewhere.

**Bad widget HMAC.** Refused flatly, with no detail about which field failed.

**Tampered or expired cookie.** Indistinguishable from being signed out.

**`SCOUT_SESSION_KEY` absent.** The auth routes do not exist; W1 serves
unchanged.

## Testing

Driven through `tower::ServiceExt::oneshot` — no socket bound, no network,
consistent with the existing suite.

- Widget HMAC verifies against a fixture; a mutated field fails; a stale
  `auth_date` fails
- A cookie round-trips; a flipped bit reads as signed out; an expired cookie
  does not authenticate
- A token consumed twice fails the second time, and a `GET` on the link
  consumes nothing — the mail-scanner case, asserted rather than hoped for
- Sign-in at an open round produces membership; at a full round, a waitlist row
- After a web claim there is a `members` row and **no** `deliveries` row;
  after a Telegram claim, both — the test that fails if the split is undone
- Linking an identity owned by another account is refused and merges nothing
- The existing 468 tests keep passing

## Configuration

Three new keys: `SCOUT_SESSION_KEY`, `RESEND_API_KEY`, `SCOUT_MAIL_FROM`.
All bot-side, so they ride the existing `.env` split into the `scout` Secret
and stay out of `scout-offsite`.

**DNS is the long pole and is not code.** Resend domain verification for
`goodscout.fyi`, plus SPF, DKIM and DMARC at Porkbun. It should be started
before implementation, not after: it is a day of waiting that can overlap the
entire build, and until it is done a magic link cannot be tested end to end.

## Deferred

- **The web chat, and core's re-keying** (W3). `run_agent(user_id, chat_id)`,
  `session::account_of(telegram_id)`, `over_daily_cap(user_id)` and
  `conversation_scope(chat_id, user_id)` all need to move onto `account_id`
  and a `(channel, address)` pair. W2 removes one piece of that coupling and
  deliberately no more.
- **Email as a delivery channel.** `deliveries` would take it without a schema
  change, and reminders by email are an obvious later step. W2 sends sign-in
  mail only.
- **The per-account daily cap.** A web session and a Telegram chat for one
  person must share a budget or the cap doubles trivially. Nothing in W2
  spends budget, so it stays W3's.
- **Session revocation.** See the decision above; an in-memory revoked set is
  the escape hatch if it becomes necessary.
- **Password authentication.** Never — both mechanisms here prove control of
  something the user already holds, and a password is a credential to store,
  leak and reset.
