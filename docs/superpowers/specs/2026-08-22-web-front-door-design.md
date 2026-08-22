# Scout on the Web — Design

## Purpose

Scout has never been reachable from the internet. The container publishes no
ports; it long-polls Telegram outbound-only, and the only way in is to be
invited to a chat. This document designs the front door, and the three phases
that follow it.

The immediate goal is small and deliberately so: a public page that describes
Scout accurately, reports whether the current invite round has room, and hands
a way in to people who arrive while it does.

## Scope

Four phases. Only **W1** is designed here in full; the rest are named so the
sequence is legible and so W1 does not accidentally foreclose them.

| | What it is | Depends on |
|---|---|---|
| **W1** | The front door: domain, TLS, an HTTP server inside the existing process, a landing page with live round state | nothing new |
| **W2** | Identity: Telegram Login Widget and email magic links, sessions, account linking, queueing on sign-in | W1 |
| **W3** | The web chat: the HTTP+SSE API and a browser client that streams | W2 |
| **W4** | The process split (formerly phase 2b-2b): the Telegram adapter stops calling core in-process and calls the same API; two containers | W3 |

The ordering inverts what the service-split spec assumed. That document had the
process split coming first, with the web app as a later consumer of an API
designed against Telegram's needs. Building the web app first means the API is
designed against a client that is **not** Telegram, which is the only real test
of whether it is channel-agnostic. W4 then shrinks to moving one existing
client onto an API that already has another.

## Decisions taken before designing

Recorded with their reasoning, because several of them constrain W2 and W3 more
than they constrain W1.

**Identity is both Telegram and email, linked by the account.** The Telegram
Login Widget maps a web visitor onto the account they already have — same
purchase history, same trips — with no password and no new identity kind. Email
magic links reach the people who will not install Telegram, which is most of
the reason a landing page exists. `identities(kind, external_id)` was built in
phase one for exactly this.

Linking happens **only while signed in**: authenticating a second way when
already authenticated attaches that identity to the current account. Two
separate sign-ups stay two separate accounts until someone deliberately joins
them. This can never merge two people by accident, which the alternatives can.

**Email is sent through Resend or Postmark.** A transactional API with a free
tier that covers a waitlist, one HTTP call from Rust, and deliverability that a
personal mailbox cannot match — a sign-in link in a spam folder is a silent
conversion failure. W2's work; see "Preparation for W2" below for the one part
worth doing during W1.

**The page states round state, not numbers.** "Invites are open" or "currently
full", never seats taken or queue length. Whether it is worth signing in is the
visitor's business; how large the userbase is, is not.

**The join link is published.** With no sign-in in W1, the open-state call to
action is a Telegram deep link carrying the round code. This makes an open round
claimable by anyone who finds the page — first come, first served, to the whole
internet. That is accepted: capacity is what limits a round, and a round that
fills from a public page has done its job. A round handed to a chosen audience
is still possible by keeping that round's code off the page.

## The constraint that shapes W1

**DuckDB is single-writer.** One process holds the database file; a second gets
a lock error rather than a second connection.

So W1 cannot introduce a separate web process. The HTTP server runs as a task
inside the existing binary, sharing the one `Core`. This is not a compromise
made for convenience — it is precisely why the process split is its own phase.
Splitting the process is the hard part, it requires every database call to
become an HTTP call, and W1 must not smuggle a fraction of that in.

## Architecture

```
internet ──► caddy ──► scout container (one process)
             :80/:443   ├─ telegram channel   (long poll, outbound only)
             auto TLS   └─ scout-web (axum, private network)
                             └─ Core ──► DuckDB
```

**A new crate, `scout-web`.** Axum routes and the page, depending on
`scout-core` and nothing from `scout-telegram`. `scout-telegram`'s `main`
spawns it alongside the dispatcher, because `main` is the composition root and
is the only place that holds a `Core`.

In W4 that spawn moves into the core binary and the Telegram side drops it. The
crate does not change when it moves, which is the point of it being a crate
rather than a module in either existing one.

**The page is embedded in the binary** with `include_str!`. One artifact, no
asset volume to keep in sync with the image, and no filesystem path to
configure or get wrong. The page is small enough that this costs nothing.

**Caddy is a new compose service and the only thing publishing ports.** It
terminates TLS with automatic certificates and reverse-proxies to the scout
container over an internal network. The scout container publishes nothing —
after W1 it is reachable through the proxy and by no other route.

### The round-state cache

The page reports open or full, which means reading `rounds()`. That read must
never touch DuckDB on a request.

The store sits behind a single mutex shared with the agent. A public,
unauthenticated endpoint that takes that mutex is a lever a stranger can pull to
make Scout slow for everyone — no exploit required, only traffic. So `scout-web`
holds the state in memory and refreshes it on a timer; a request reads a value
and returns.

This mirrors the members `DashSet` the Telegram gate already uses, for the same
reason: the gate runs on every update including from strangers, so it must not
reach the database.

The interval is **30 seconds**. Accepted consequence: opening or closing a round
takes up to that long to appear on the page — invisible to a human, and it makes
every request free.

### What it serves

```
GET /          the page, with round state substituted at request time
GET /healthz   liveness, for the proxy and for the deploy script
```

No JSON endpoint. An earlier draft had `GET /v1/rounds/current` returning state
for the page to fetch; server-side substitution is simpler, works with
JavaScript disabled, and nothing consumes JSON until W3 has a client that needs
it. One less public surface until there is a reason for it.

## The page

Content leads with **shopping**, not flights. That is what Scout is used for,
and the flight side is a working prototype that should be described as one.

Sections, in order:

1. **Hero.** What Scout is, and a price comparison as the proof: three offers
   where the cheapest sticker price is the worst deal once shipping and
   price-per-unit are in. The argument the product exists to make, made once,
   in a monospace block.
2. **How it finds the cheapest.** Landed cost rather than sticker; price per
   unit; prices read from a page's structured data rather than its text.
3. **Your shops, not just the ones that rank.** Favourite shops scoped by
   category, reached with a `site:` query because small shops never rank
   organically. Local-language search in the same breath.
4. **Where the listings come from.** Each source labelled by what it actually
   is — see below.
5. **Every row here was a real bug.** Three failures and their fixes.
6. **Flights, honestly: a prototype.** Labelled as such.
7. **Status strip and call to action.**

### Naming the sources accurately

This section states publicly how each marketplace is reached, and the
distinctions are real:

- **eBay — official API.** The Browse API with developer credentials and OAuth.
- **Marktplaats — unofficial feed.** The same JSON endpoint marktplaats.nl's own
  front end calls. Not scraping rendered HTML, but not a public API either: the
  code's own comment records that its shape may change, and that failures
  degrade to ordinary search.
- **bol.com — official API.** The Catalog API, with an approved affiliate
  account.
- **Everywhere else — read and verified.** Found by search, opened, and read;
  headless Chrome for pages that refuse a plain HTTP client.

Describing Marktplaats as a "live API" alongside eBay would be the kind of
small overstatement this whole product exists to avoid. A page whose argument is
"we do not overstate things" cannot afford one.

### Visual design

Solarized dark, committed to — a single look, no light variant.

| Role | Colour |
|---|---|
| Page background | base03 `#002b36` |
| Card and block background | base02 `#073642` |
| Body text | base0 `#839496` |
| Headings, emphasis | base1 `#93a1a1`, base2 `#eee8d5` |
| Muted, captions, labels | base01 `#586e75` |
| Primary action | blue `#268bd2` |
| Wordmark, accents | cyan `#2aa198` |
| Round open, correct behaviour | green `#859900` |
| Round full, shipping costs | orange `#cb4b16` |
| Failures in the bug table | red `#dc322f` |

Responsive at 680px: columns collapse, the comparison table becomes stacked
pairs, the hero scales down.

## Security posture

W1 is the first time Scout is reachable from the internet, so the posture is
worth stating rather than assuming.

- **No input of any kind.** No forms, no query parameters that reach code, no
  cookies, no session. The injection surface is nil, which is the right shape
  for a first exposure.
- **No secrets on the page.** The round code is published deliberately; nothing
  else from the environment reaches the response.
- **The application container publishes no ports.** Only Caddy is exposed.
- **Security headers at the proxy**: HSTS, `X-Content-Type-Options`,
  `X-Frame-Options`, a content security policy tight enough to matter given the
  page loads no external resources and runs no scripts.
- **The database is not on the request path**, by the cache above. This is the
  main denial-of-service consideration and it is designed out rather than
  mitigated.

## Testing

Routes are driven through `tower::ServiceExt::oneshot` — no socket bound, no
network, consistent with the existing suite's refusal to touch either.

- An open round renders the join path; a full round renders the closed state.
- The cache serves without a database read, and a refresh picks up a change.
- `/healthz` answers.
- The existing 446 tests keep passing.

## Deployment

`compose.yaml` gains a `caddy` service publishing 80 and 443, with a `Caddyfile`
and volumes for certificates. The `scout` service joins an internal network and
publishes nothing.

`scripts/deploy.sh` needs no structural change; its dirty-path list gains the
`Caddyfile`. The domain is configuration, not code.

The first deploy of W1 is the first that can fail in a new way — a certificate
that will not issue, DNS that has not propagated — so it should be done when
there is time to watch it, and the rollback is removing the proxy service, which
leaves the bot untouched.

## Preparation for W2

One thing is worth doing during W1 because it takes wall-clock time rather than
work: **the SPF and DKIM DNS records for the email sender**, set up while the
domain is being configured anyway. Records propagate slowly; having them in
place means W2's email code is testable the day it is written rather than
blocked on a DNS change. This is configuration only — no code, no dependency, no
scope moved forward.

## Deferred, and why

- **Queueing from the web.** W1 has no sign-in, so a visitor who arrives at a
  full round is told it is full and offered nothing else. This is a known dead
  end, accepted because the queue is account-based — `waitlist` keys on
  `account_id` and `waitlist_to_invite` returns people with somewhere to reach
  them — and an email address with no account behind it would be a second list
  invisible to the machinery that already works. W2 closes it.
- **Rate limiting.** The cache removes the database from the request path, which
  is the consideration that matters. Bandwidth and CPU limits can wait for
  evidence that anyone is pointing traffic at this.
- **The daily cap becoming per-account rather than per-channel.** A web session
  and a Telegram chat for the same person must share one budget or the cap is
  trivially doubled. This is the `(channel, address)` change already recorded as
  deferred by the workspace phase; W2 is where it stops being deferrable.
- **Core owning Telegram's `/invite` grammar.** Recorded by the workspace phase.
  It wants fixing before W3 designs an endpoint around `InviteCmd`, or one chat
  client's command grammar becomes the protocol between services.
