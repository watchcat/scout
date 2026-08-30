# W2 Security Review — Findings and Remediation

An adversarial review of the sign-in code after it went live on
goodscout.fyi at `f611c97`. Findings are the reviewer's, verified
independently where marked. Ordered by severity.

The two takeover paths share one root cause: **the code checks that a request
carries a valid credential, and never checks that the user meant to make it.**
Everything cryptographic is sound; every hole is on that line.

## 1. Cross-site GET on `/auth/telegram` links an attacker's identity — takeover

`routes/auth.rs:40`, `:271-284`. **High. Live. Verified independently.**

`/auth/telegram` is a state-changing `GET` with no CSRF token, no `state`
parameter and no `Origin` check. `SameSite=Lax` sends the session cookie on
top-level cross-site navigations, so:

1. The attacker presses the real widget and copies their own signed payload
   out of the address bar.
2. Within the 60-second `auth_date` window, a signed-in victim is navigated
   to `/auth/telegram?<that payload>`.
3. `signed_in_as` returns the victim; the attacker's Telegram id is linked to
   the victim's account.
4. The attacker signs in with the widget and is inside the victim's account.

`PRIMARY KEY (kind, external_id)` does not help: the victim's account simply
ends up with two `telegram` rows.

**Fix.** Put a signed `state` in the widget's `data-auth-url` and require it
on the callback. It must be bound to the session when one exists, so a state
minted for a signed-out browser cannot be replayed into a signed-in one.

**Why the plan produced this.** `account.rs` binds CSRF tokens to the account
for `/sign-out` and `/account/link/email`, where a forgery is a nuisance,
while the one endpoint that grants a permanent credential had none. That
asymmetry came from the plan, which named CSRF only for `POST`.

## 2. The anonymous CSRF token is harvestable, so `POST /auth/email` is open to login CSRF

`session.rs:100-108`, `routes/auth.rs:82`, `:184`. **Medium-high. Verified.**

The pre-session form token is minted by a public `GET /sign-in`, is not
one-time, and is valid for 15 minutes for everyone — so it proves nothing
about where the POST came from. An attacker harvests one, requests a link to
their own address, and auto-submits it from a page the victim visits. The
victim's browser receives a session cookie for the **attacker's** account,
and anything the victim then attaches lands there.

The existing test proves only that an *absent* token is refused, which no
attacker would do.

**Fix.** Check `Origin` (falling back to `Referer`) on every state-changing
POST, and reject cross-site ones. A same-origin check is what the token was
standing in for.

## 3. A stranger can take the store mutex

`routes/auth.rs:134-139` → `store.rs:1245`. **Medium. Verified.**

`POST /sign-in/email` is unauthenticated and writes a row through the single
`Arc<Mutex<Connection>>` the bot itself uses. The design's claim that
"a stranger cannot make the site take the store mutex" is true of session
*verification* and false of sign-in. A flood degrades the bot for real users.

**Fix.** Cannot be removed without a queue; reduce it by making the limits in
finding 4 actually bind, and state the residual honestly in the design.

## 4. The abuse limits do not bound anything an attacker cares about

`routes/auth.rs:114-121`, `ratelimit.rs:24-44`. **Medium. Verified.**

- Per-address is per *string*: `v+1@x`, `v+2@x`, `v.i.c@x` are separate keys
  reaching one inbox, so the cap does not stop a targeted mail bomb.
- Per-IP keys on the full address, so any IPv6 client owns a /64 of buckets.
- `login_tokens` is never pruned — one row per request, kept forever by
  design so `AlreadyUsed` stays distinguishable from `Expired`.
- `Limiter::allow` sweeps the entire map on every call under a
  `std::sync::Mutex` on a tokio worker: O(n) per request with n grown by the
  attacker.
- Each accepted request builds a fresh `reqwest::Client` — a new TLS config
  and pool per email.

**Fix.** Normalise the address key (lowercase, strip `+tag`, strip dots for
the known providers that ignore them). Key IPv6 by /64. Sweep on a schedule
rather than per call. Share one `reqwest::Client`. Prune `login_tokens` older
than a day in the existing maintenance loop — consumed rows only need to
outlive their own expiry to keep the two messages apart.

## 5. `client_ip` trusts the first `X-Forwarded-For` entry

`routes/auth.rs:330-339`. **Medium, deployment-dependent. Code verified.**

Taking `.split(',').next()` is right only if the edge overwrites the header.
The comment says a client can put anything in front of it, and then takes
that value. Probably not exploitable behind the current Traefik ingress,
which strips untrusted forwarded headers — but it is one ingress change from
being live, and `is_none_or` means a missing header is "allowed" rather than
"throttled".

**Fix.** Take the entry the trusted proxy appended (the last), not the first.

## 6. The session MAC's coverage is untested

`session.rs:28-56`. **Medium (test quality). Demonstrated by mutation.**

Signing only `account_id` — dropping `expires` and `nonce` from the MAC —
passes the entire suite. Under that mutant anyone edits `expires` and holds a
session that never dies. The production code is correct; the safety net is
not there.

`the_account_id_cannot_be_edited_without_breaking_the_signature`
(`session.rs:165-172`) is **vacuous**: it never edits an account id. It mints
two cookies, asserts they differ, and asserts the second verifies — all of
which passes against a `verify` that does no signing at all.

**Fix.** Rewrite it to tamper with each field in turn — account id, expiry,
nonce — and assert each is refused.

## 7. `AuthConfig::from_env` treats an empty variable as set

`lib.rs:77-91`. **Low.**

`env::var("X").ok()?` returns `Ok("")` for `X=`. Four of five keys pass when
empty, which is the half-configured deployment the comment says it prevents.
`scout-core`'s `Config::from_lookup` filters empties; the two disagree.

**Fix.** Filter empty and whitespace-only, matching `config.rs`.

## 8. A refused link looks like a success, and spends the token

`routes/auth.rs:216-222`. **Low.**

`Err(e)` and `Ok(LinkOutcome::TakenByAnother)` both redirect to `/account`
with no note. The Telegram path carries `?linked=taken` for this case and the
machinery exists; the email path does not use it.

## 9. Smaller items

- **low** — The magic-link token rides in a URL, so it is in history and
  proxy logs; the confirm POST binds to no cookie or nonce, so with finding 2
  anyone reading such a log can spend it. `Cache-Control: no-store` is not set.
- **low** — No `no-store` on the signed-in half; `/account` carries standing
  and a live CSRF token.
- **low** — Cookie has no `__Host-` prefix, so a foothold on a sibling or
  parent domain can shadow it.
- **low** — No HSTS on the live site. The `Caddyfile` sets it; the k3s
  ingress that is actually in front does not. Related to the HTTP→HTTPS
  redirect already recorded as unfinished in the k3s plan.
- **nit** — `telegram_login::verify` bounds only the past; a future-dated
  `auth_date` is accepted forever.
- **nit** — `identity_kinds` can return duplicates, so after finding 1 the
  page reads "Signed in with Telegram, Telegram."
- **nit** — `consume_login_token` writes `consumed_at = current_timestamp`
  (local) while `expires_at` is UTC-naive — the same mismatch documented on
  `requests_today` as having caused a real bug. Harmless today because the
  column is only null-tested.

## Confirmed closed

Cookie forgery and re-delimiting; CSRF/session key separation in both
directions; `GET` on a link consuming nothing; `Expired` and `Unknown`
rendering identically; the link form not being a membership oracle; the
60-second Telegram replay bound; `link_identity` never moving an identity;
HTML escaping on every interpolation traced; open redirect.

---

# Resolution

All findings closed on branch `w2-security-fixes`, 20 commits. 547 tests,
clippy clean.

| # | What closed it |
|---|---|
| 1 | A signed `state` on the widget's `data-auth-url`, bound to the session when there is one. The attack was written as a test, confirmed to succeed against the old code, and now returns 400 with nothing linked. |
| 2 | An `Origin`/`Referer` check on every state-changing POST — plus `Referrer-Policy: strict-origin`, without which the check was toothless. See below. |
| 3 | Not closed; cannot be without a queue, which was not built. The design document now states the residual instead of claiming otherwise. |
| 4 | Address keys normalised, IPv6 keyed by /64, the map swept on a size threshold rather than per call, one shared `reqwest::Client`, and `login_tokens` pruned hourly in `run_maintenance`. |
| 5 | `client_ip` takes the last `X-Forwarded-For` entry. A request with no forwarded header now shares one bucket rather than being exempt. |
| 6 | The vacuous test rewritten to tamper with each payload field in turn. The reviewer's mutation was reapplied to confirm the suite passed under it before, and that the new test fails under it now. |
| 9 | `__Host-` cookie prefix, `Cache-Control: no-store`, HSTS on the whole router, a two-sided `auth_date` window, `SELECT DISTINCT kind`, and `consumed_at` on the same clock as `expires_at`. |

## Two things worth carrying forward

**Finding 2 was nearly fixed in name only.** The `Origin` check had to allow
requests naming nobody, because `Referrer-Policy: no-referrer` makes browsers
send `Origin: null` on form posts — so our own forms looked nameless and an
attacker who set the same policy looked identical to us. The header and the
check are one decision. `strict-origin` keeps the mailed token out of other
people's logs, which is the only thing `no-referrer` was for, while leaving
our own posts a real `Origin`. A test fails if either is changed without the
other.

**The agent that fixed it said so itself**, rather than reporting the finding
closed. That is the only reason it was caught.

## Still open

- **Finding 3's root cause.** `POST /sign-in/email` still writes to DuckDB
  through the bot's own mutex. Bounded now, not removed.
- **Signed-out login CSRF on `/auth/telegram`.** A state for a signed-out
  browser is fetchable from the public `/sign-in` by construction, so an
  attacker can still push a *signed-out* visitor into signing in as the
  attacker's Telegram identity. The signed-in takeover is closed.
- **A signed-in user can link a second Telegram identity.** `/account` hides
  the widget once you have one; `/sign-in` does not. Possibly intended —
  someone with two Telegram accounts — but the two pages disagree, and that
  disagreement is what made the duplicate render reachable.
- **The widget's `data-auth-url` now carries a query string** and nobody has
  confirmed Telegram honours that. It fails visibly if not.
