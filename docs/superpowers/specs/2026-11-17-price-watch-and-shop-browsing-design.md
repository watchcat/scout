# Watching a Price, Browsing a Shop, and Who Pays — Design

## Purpose

A proposal of 2026-11-17 asked for three things in one architecture: run
untrusted code for people in Firecracker microVMs, research several shops in
parallel with a browser, and let Scout check out — "find the Sony
WH-1000XM5 on Amazon.nl and buy it if it is under €300" — with a vaulted
card typed into the shop's form.

This document keeps what the person in that sentence wants and drops the
machinery Scout cannot run or should not hold. In the order it is worth
building:

1. **"Tell me when it is under €300."** A price watch: Scout reads the price
   once a day, in Rust, and sends one message when it is at or under the
   target, with the link.
2. **Prices that need a click.** A variant's price, the shipping to a
   postcode, a delivery date — numbers a page shows only after something is
   pressed. A shop desk that drives a browser, if two measurements say it is
   needed and possible.
3. **The purchase stays with the person.** Scout ends where it ends today,
   at a verified link — as close to the pay button as the shop allows.

## What was measured first

All on 2026-11-17 unless dated otherwise.

| fact | where it comes from |
|---|---|
| The production node is itself a virtual machine without nested virtualization: no `/dev/kvm`, no `vmx`/`svm` flag. Firecracker cannot start on it. | the node |
| 4 vCPUs, 7 GB of memory; Scout's pod is limited to 3 Gi; two page renders at once is the ceiling already (`MAX_CONCURRENT_RENDERS`). | the node, `deploy/k8s/deployment.yaml`, `tools/browser.rs` |
| 143 runs in 33 days — four a day. | `docs/2026-10-16-search-fanout.md` |
| A Chrome driven over the DevTools protocol announces itself, and a challenge page never clears for it (action.com: 67 s, still the interstitial). The same Chrome as a one-shot subprocess passes every time. | `tools/browser.rs`, `render_inner` |
| Chromium runs as a child of Scout: same pod, same uid, `--no-sandbox`, with Scout's environment — every key — and the database file within reach. | `tools/browser.rs`, `chrome_args` |
| k3s runs with its default flags, so its network policy controller is on. There are no policies. | the node |
| "It never buys anything … the purchase stays in your hands." | `README.md` |

## Decisions taken

- **Scout does not pay, hold a card, or sign in to a shop.** Not as a
  matter of caution but because the proposed design does not hold:
  - the card would be typed into a browser inside the machine the design
    itself calls untrusted, and so would the person's shop session, which
    is worth more than the card;
  - the next page outline or screenshot after the fill carries the card
    number to the model provider;
  - every check the host makes — the total, the merchant — it makes
    through that same untrusted machine, so a compromised one reports what
    it likes; the only check it cannot fake is the bank's own approval
    screen, and banks skip that screen for small and low-risk payments;
  - a vault that hands a card number back puts Scout in PCI scope, and the
    security code may not be kept once a payment is authorised, so it is
    never on file for the next one;
  - shops forbid automated buying and act on it; the account at risk is
    the person's.

  Revisit only for a payment the bank or card network itself binds to an
  amount and a merchant on the person's approval (the agent payment tokens
  the networks and processors were introducing in 2025 and 2026), at a merchant that
  accepts them. Never by typing a card or holding a login.
- **No code execution for people.** Nobody has asked Scout for anything
  that needs it. Revisit when three different people have; rent a hosted
  sandbox first, since microVMs of Scout's own need a host with KVM.
- **No microVMs. The fence is a pod.** What needs isolating is Chromium,
  and a pod with no secrets and a network policy isolates it on the node
  Scout has.
- **Delegation is the `Specialist` that exists.** The flight desk is the
  pattern: a nested agent behind one tool, a self-contained brief in,
  findings and guidance out, a budget, the outer run's observer for
  progress and token counts. The shop desk is a second instance, not a new
  engine.
- **Two measurements gate the browsing half.** Whether people need prices
  Scout cannot read, and whether a driven browser can read them. The
  thresholds are named below, before measuring.
- **No disguise.** Scout sends a normal browser's user agent and nothing
  more. A shop that refuses automated sessions is a shop the desk does not
  browse; Scout does not work around a challenge or a CAPTCHA.
- **Accounts, not tiers.** Everything here is keyed by `account_id` and
  open to every member. A paid tier is a separate decision with a billing
  design of its own.

## Part 1 — Price watch

Independent of everything else in this document, and the only part with no
gate.

### What the person sees

> **Tell me when these are under €300.**
> Watching Sony WH-1000XM5 at bol.com: €329.00 now, I will tell you when it
> is €300.00 or less. I check once a day until 16 January.

and some days later, once:

> Sony WH-1000XM5 is €289.00 at bol.com — under the €300.00 you asked for.
> It was €329.00 on 17 November. https://www.bol.com/…

### Tools

Three tools on the main agent, offered only when the run has somewhere to
deliver to — the rule `create_reminder` already follows, for the same
reason: a watch is a promise to come back.

- `watch_price { url, target, currency }` — fetches the page itself, through
  the `fetch_page` path (plain request, then the render fallback, both
  behind the guard), and needs a machine-readable price: a `PageProduct`
  with a price and a currency from the extractors in `tools/fetch.rs`. The
  model never supplies the current price. It refuses, saying why, when
  - no price can be read from the page — a watch that cannot be checked is
    not started;
  - `currency` is not the page's — Scout does not compare across
    currencies;
  - the price is already at or under the target — it says so instead;
  - the account has ten live watches.
- `list_watches {}` — what is being watched, with the last price read.
- `stop_watch { id }`.

### `price_watches` (schema step 27)

`id`, `account_id`, `url`, `title`, `currency`, `target` and `first_price`
(`DOUBLE`, as every price column is), `last_price`, `lowest_price`,
`last_checked_at`, `unreadable_streak`, `created_at`, `expires_at`,
`ended_at`, `ended_why` (`hit`, `expired`, `stopped`, `unreadable`).

### The daily check

In core's hourly maintenance tick. For each live watch not checked in the
last twenty hours:

1. Read the page the way `watch_price` did. No model call: a check costs
   HTTP and nothing else.
2. A price read: store it; when it is at or under the target, end the watch
   as `hit` and queue the message.
3. No price: add to the streak. Three in a row end the watch as
   `unreadable`, with one message saying the page can no longer be read.
4. Past `expires_at` (sixty days): end as `expired`, with one message
   giving the lowest price seen.

Requests to one host are spaced five seconds apart. Every message goes
through the outbox the way an inbox nudge does (`inbox::nudge`), keyed
`watch:<id>:<why>`, so it is sent once whatever is retried. The comparison
and the amounts in the message are Rust's; the text is a template.

Ended watches are pruned ninety days after they end, in maintenance.

`/metrics`: `scout_price_watch_checks_total{outcome}` (`read`, `unreadable`,
`refused`) and `scout_price_watches_ended_total{why}`.

### What it does not promise

The price is the page's own machine-readable one. Where a page states a
"from" price, another variant's, or a marketplace seller's, that is the
number watched; the first answer names the price it read, so a wrong one is
visible on the day the watch starts.

## Part 2 — Chromium in a pod of its own

Worth doing whether or not the desk is ever built: today a renderer that an
attacker's page takes over is a process with Scout's uid, Scout's keys in
its environment and Scout's database a file away.

- **`scout-browser`**, a second Deployment from the same image, running a
  small server (a new crate in the workspace, `scout-browser`): `POST
  /render {url}` returns what `Renderer::render` returns today, by the same
  one-shot `--dump-dom` subprocess, behind the same guard and the same two
  slots.
- **Nothing to steal:** no `envFrom`, no service-account token, non-root,
  read-only root with a memory-backed scratch volume for profiles, all
  capabilities dropped, memory limited to 1.5 Gi.
- **Nowhere to go:** a `NetworkPolicy` on the pod. In: Scout's pod only.
  Out: DNS, and the public internet with the private, link-local and
  cluster ranges excepted — so not Scout's `:9090`, not the Kubernetes API,
  not the node. The guard inside stays as a second fence, but the policy is
  the boundary: it holds when the process behind the guard does not.
- **Scout's side:** `Renderer` gets a remote form, used when
  `SCOUT_BROWSER_URL` is set; the subprocess form stays for local runs.
  What comes back is untrusted text, as a fetched page always was.
- **Proved, not assumed:** `scripts/check-browser-fence.sh`, run after a
  deploy — from the browser pod, a request to Scout's service and to the
  API server fails, and one to a public page succeeds.

The trip PDF stays where it is: it prints Scout's own page, not the web.
The existing board card "`NetworkPolicy` for egress" keeps Scout's own
egress; this takes the browser out of it.

## Part 3 — Browsing a shop (gated)

### Gate 1 — what the traces say

`scripts/unread-pages.py`, over the nightly backup, sixty days, written up
as `docs/<date>-unread-pages.md` the way the fan-out reading was:

- `fetch_page` calls by host: read with a price; read only after a render;
  blocked; read with no price;
- offers handed to `compare_prices` with no shipping cost, by host —
  "absent means not stated" is already how that tool records it.

A write-up is a file anyone can read, and nearly every host in the traces
is one person's. So a write-up names a host only when two or more
accounts used it and gives the rest as sums or unnamed rows; the operator
has the names. This was added after the reading
(`docs/2026-11-18-unread-pages.md`), which found 183 of 189 hosts used by
a single account.

### Gate 2 — what a browser can reach

For the ten hosts Gate 1 chose, from the production node (its address
is the one shops see), three ways of reading one product each: the plain
request, the one-shot render, and a driven session. For each: the price, the
price of a chosen variant, shipping to a Dutch postcode, a delivery date —
and whether the shop refused the session. The driven session is a throwaway
prototype of the loop below, so the same run reports turns, tokens and
seconds per shop. A laptop's Chrome through an SSH tunnel to the node is
enough for this — nothing is installed there and the shops still see its
address — with the image's Chromium deciding wherever the two disagree.
Written up as `docs/<date>-shop-reach.md`, with a second
table: which of the ten have a link that preselects a variant or fills a
cart.

### The rule, named before measuring

Build sessions and the desk only if both hold:

- **needed** — in Gate 1, at least one `fetch_page` read of a product page
  in ten returned no price, or at least one offer in three reached
  `compare_prices` with no shipping cost; and
- **possible** — in Gate 2, on at least three of the ten hosts a driven
  session gets a number the other two ways do not, and the shop does not
  refuse it.

Otherwise the cards below close with the measurement as their result, as
the prompt-cache card did.

### Browser sessions

In `scout-browser`, driven over the DevTools protocol inside the pod, so the
protocol never crosses the network. Scout speaks to it over HTTP: open a
session on one page, act, read, close.

- **Actions:** `open`, `read`, `click`, `type`, `select`, `back`. Each
  returns the page as an outline: title, address, and up to 6,000
  characters of visible text with the things that can be pressed numbered.
- **One site:** a session is opened on one shop. A main-frame navigation
  that leaves that shop's registrable domain is refused and reported.
- **Nothing personal typed:** `type` refuses password and payment fields
  and anything the page marks as a name, an email, a phone number or a
  street address, and takes at most 64 characters. What is left is a search
  term, a quantity, a postcode.
- **No side doors:** downloads, uploads and permission prompts are off. A
  session is one private context, gone when it closes.
- **Bounded:** three sessions at once, 25 actions and 120 seconds each. A
  call that finds no free slot waits inside its own budget, as a render
  does. The pod's memory limit goes to 2 Gi with this.

These rules are against a model led astray by a page, not against a
compromised pod; for that there is the fence of Part 2, and the fact that
the only thing a session ever holds is the brief.

### `ask_shops`

A `Specialist`, offered when `SCOUT_BROWSER_URL` is set and `SHOP_DESK` is
on, through `available_tools` as `ask_flights` is.

- **In:** a brief and one to three product pages, which the main agent
  found by its own search. The desk does not search the web. A postcode is
  in the brief only when the person gave one for this.
- **Inside:** one nested run per page, concurrently, each with a session and
  six tools — the five actions and `report_offer { title, variant, price,
  currency, shipping, delivery, url }`. No memory, trips, reminders or
  fetch: nothing with an effect outside its own session.
- **Checked in Rust:** `report_offer` is accepted as read only if its
  amounts are written in the last outline the session returned
  (`text_writes_amount` does this for embedded prices today) and its
  address is the session's. Otherwise the finding says unverified.
- **Out:** the offers as findings, and guidance: hand them to
  `compare_prices`; a price is a quote as of the minute it was read; say
  which could not be verified.
- **Budget:** twelve turns a page, all pages inside the one specialist
  budget. Tokens are counted by the observer as the flight desk's are.
  `SHOP_MODEL`, defaulting to `LLM_MODEL`.
- **Who:** `SHOP_DESK=admins` first, then `all` after a two-week reading of
  runs, turns, tokens, seconds and failures by host.

An injected page can waste the budget or misstate its own shop's price. It
cannot reach a key, another account, a tool with an effect, or another
site; and the amount check, the link and the time on the quote are what the
person and the main agent hold against the second.

## Part 4 — Cart links

Where Gate 2's second table shows a shop has a stable link that preselects a
variant or fills a cart, an offer from that shop carries it. Built by a
function per host in Rust, only from identifiers read off the page — never
a link the model wrote, which is the invented-ASIN problem `tools/shown.rs`
describes. A shop with no such link gets the product link, as today. If the
table is empty, this part closes with it.

## What the proposal asked for, and where it went

| proposal | here |
|---|---|
| VM-optional by default; ordinary runs untouched | kept: nothing changes for a run that watches no price and asks no desk |
| A subagent that returns only findings | kept: `Specialist`, which exists |
| No secrets where untrusted pages run | kept, and extended to today's render (Part 2) |
| Approval before money moves | moot: no money moves |
| Budgets and concurrency caps | kept: slots, turns, seconds, the specialist budget |
| A stand-in backend for tests | kept: the subprocess renderer stays for local runs |
| Firecracker, snapshots, vsock enrolment | dropped: no KVM on the node; a pod and a policy do the job |
| Bash and Python for people | dropped until someone needs it |
| A proxy lending keys to guest code | dropped with the guest code |
| Card vault, blind fill, 3DS as the wall | dropped: see Decisions |
| FREE / PRO / ADMIN | dropped: accounts and rounds exist; billing is its own design |
| "Buy it if it is under €300" | Part 1 and Part 4: told when it is, one press from the cart |

## Stories

In build order. Each is a board card.

1. **Price watch.** Done when: asking to be told about a price starts a
   watch only on a page whose price Rust can read; the daily pass sends
   exactly one message on a hit, on three unreadable days, and on expiry;
   watches can be listed and stopped in conversation; an account without a
   delivery address is not offered the tools; ten live watches is the cap;
   the counters are on `/metrics`; the README says what a watch is.
2. **What the traces say about pages Scout cannot price.** Done when the
   script and the write-up exist, and the ten hosts are chosen: named in
   the write-up where two or more accounts used them, with the operator
   otherwise.
3. **Shop reach.** Done when the write-up has both tables for those ten
   hosts, with turns, tokens and seconds for the driven sessions, and says
   whether the rule above is met.
4. **Chromium in a pod of its own.** Done when production renders through
   `scout-browser`; the pod has no secret, no token and the policy; the
   fence script passes after a deploy; a render still returns what it
   returned before; local runs are unchanged.
5. **Browser sessions.** *Gated on 3.* Done when a session can be opened,
   driven and closed over HTTP; every refusal above has a test; the limits
   hold under two concurrent sessions.
6. **The shop desk.** *Gated on 3; needs 4 and 5.* Done when `ask_shops`
   returns verified offers for fixture shops and marks an invented price
   unverified; it is off without `SCOUT_BROWSER_URL`; it runs for admins in
   production; the reading after two weeks is written.
7. **Cart links.** *Gated on 3's second table.* Done when an offer from a
   shop in the table carries its link, built in Rust, and one from any
   other shop is unchanged.

## Testing

As the rest of Scout: a failing test first. The watch against fixture pages
and a store on a temporary file, with dates counted from today rather than
written down. The fence by the script, against the real cluster. Sessions
against pages the test serves itself. The desk with a scripted model, as the
specialist's tests do, and source tests that it holds no tool with an
effect.

## Open questions

- **A watch for someone with no Telegram.** Not offered, as reminders are
  not. Email is a channel Scout already has for them; whether a watch
  should use it is a product decision, not made here.
- **Sixty days and ten watches** are first guesses; the counters will say.
- **`--no-sandbox`.** In a pod of its own, Chromium's own sandbox may work;
  worth one try in story 4, not a condition of it.

## Out of scope

Paying, holding a card, signing in to a shop; running code for people;
microVMs; tiers and billing; a browser the person steers; watching anything
but a price (stock, a delivery date); more than three pages a desk call.
