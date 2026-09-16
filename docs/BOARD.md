# Board

What is being worked on, what is next, and what is waiting. One line per
card; the design docs and plans under `docs/superpowers/` carry the detail.
Move a card by moving its line. Add the date when a card lands in **Done**.

## In progress

_(nothing)_

## Next

- [ ] **Chat sees pending arrivals** — the trip view in chat marks arrivals waiting on the Trips tab, from their extracted fields only (deferred from the booking-address slice)
- [ ] **Show the stored mail text** — the Other-mail row's click shows the message text and attachments (stored, capped at 512 KB, not yet served)
- [ ] **Poller liveness** — record the last successful `getUpdates`; fail `/healthz` when it is stale so k8s restarts a bot Telegram has gone quiet on (24 restarts in 20 minutes happened once with nothing to catch it)
- [ ] **Retention for the rest** — `outbox` never deletes sent rows, `request_log` never prunes; two DELETEs in `run_maintenance` next to the thread expiry
- [ ] **CI** — `.github/workflows`: `cargo test`, `cargo clippy -D warnings`, `cargo audit`, `node --test 'crates/scout-web/src/*.test.mjs'`, cached with `Swatinem/rust-cache`
- [ ] **Timeout on `continues_previous`** — same shape as `TITLE_BUDGET` on `title_for`; this one runs on every Telegram message after a 10-minute gap and has no bound
- [ ] **Specialist deadline from the remaining run budget** — `SPECIALIST_BUDGET` is a fixed 180 s; two `ask_flights` calls in one request can outlast `RUN_BUDGET` (300 s), and the wrap-up builds its notes from streamed text only, so a report already paid for reaches neither the wrap-up nor history. Compute the deadline from what is left, and feed the last report into the wrap-up notes

## Backlog

### Scaling
- [ ] **Lock-wait tracing** — a span around every store call with the time spent waiting on the mutex; the numbers decide the next card
- [ ] **DB decision** — stay on DuckDB with read-only `try_clone()` connections, or move to SQLite (`rusqlite` bundled, WAL) — SQLite also removes the 10-minute C++ build that shapes the whole deploy pipeline
- [ ] **Metrics** — a `/metrics` endpoint: runs, run duration, tool calls, provider errors, store lock wait
- [ ] **Webhook mode** — long polling and the single-writer DB both pin `replicas: 1`; needed before a second replica, not before 1000 users
- [ ] **Per-provider rate limiter** — retry covers a 429 after the fact; a token bucket per provider would avoid earning one

### Cost
- [ ] **Token usage per run** — log input / cached / output tokens into `request_log` from rig's final response; until then every model-cost comparison is arithmetic on guesses
- [ ] **Cache-friendly prompt** — the per-user profile facts sit inside the system prompt and break the prefix cache per user; move them to the end
- [ ] **Provider config** — `LLM_BASE_URL` / `LLM_MODEL` / `LLM_API_KEY` so a second provider is a config change (`MINIMAX_BASE_URL` exists; the model name does not)
- [ ] **GPT-5.6 Luna trial** — postponed 2026-09-05. Roughly 30% cheaper than MiniMax M3 on list price, but reasoning tokens may eat it, and Luna needs the Responses API for tools + reasoning. Try it on the tool-less side calls first, once usage logging exists
- [ ] **Search fan-out** — Kagi at up to 15 queries per run is the dominant cost, an order of magnitude above the model; measure how often the later queries add a result

- [ ] **Repair turn inside the flight desk** — a tool call written as prose in the nested run is now reported as a failure (`7356fa7`); one `REPAIR_NOTE` turn inside the nested run, as `run.rs` does for the main agent, would keep the research instead

### Hygiene
- [ ] **Duplicate dependency trees** — reqwest 0.12 + 0.13, rand 0.8/0.9/0.10, sha2 0.10/0.11; bumping reqwest to 0.13 (what rig uses) drops one TLS stack
- [ ] **`proc-macro-error2` future-incompat** — transitive; check for an updated upstream
- [ ] **Backup restore drill** — the restic repository has never been restored from; one rehearsal on a scratch node
- [ ] **Concurrency doc** — `docs/2026-08-08_225900-scout-100-concurrent-users.md` is in Russian while everything else is English, and most of it has shipped; rewrite as a status page or retire it
- [ ] **`target/` is 49 GB** locally — `cargo sweep` or a periodic clean

### Threads follow-ups (from the reviews)
- [ ] Drop the dead `conversations.pending_draft` column — nothing reads it; the live draft is in the Telegram adapter

## Done

- [x] 2026-09-16 — **Throw away mail you are done with** (`68245ca`): an x on each Other-mail row deletes that message now rather than waiting out the thirty days, following the retention sweep's own rules — the readings and the attachments go, except one already attached to a trip item. Refused while a booking of it is still waiting on a trip, or while the worker is still reading it, with the reason shown in the row. The confirm replaced a button on its own pixel, so a double-click pressed Delete unread; the destructive half is dead for half a second, here and on the Remove that takes a leg off a trip.
- [x] 2026-09-16 — **A ticket you hold looks like one** (`12365f7`): a flight added from a forwarded confirmation read "No flight saved yet". The extractor reads the airline, flight number, both local clocks and any stop; step 17 keeps them; the leg gets a chosen option marked as coming from the mail, so the card, the timeline and the connection check see the flight and a connecting ticket is not labelled direct. Matching also reads the bookings waiting on a trip, so a second email joins the first one's draft. A draft nothing landed on is collected once it is five minutes old. No duration is written: two clocks in two cities said 18h30 for an 11h30 flight.
- [x] 2026-09-16 — **One email, many bookings** (`b54d763`): a round-trip ticket confirms two flights in one email and the extractor answered with one JSON object, so a live AMS–HKG / HKG–CDG–AMS booking produced only the outbound. The model answers with a list now, told that a connection is one booking and a return a second, and that the ticket total belongs on the first entry alone; every booking from one email lands on one trip.
- [x] 2026-09-16 — **A booking address for every account** (`7461465`): `<handle>@goodscout.fyi` via Resend inbound. A signed webhook stores the mail; a worker forwards it to the person's own email, reads it with one tool-less model call into an arrival, places it on the overlapping trip or a new draft, and nudges Telegram. The Trips tab shows dashed pending rows with Add / Not this trip / Ignore; Other mail lists the rest for thirty days. Off until `RESEND_WEBHOOK_SECRET`, the Resend receiving domain (MX at Porkbun) and the `email.received` webhook exist. Spec: docs/superpowers/specs/2026-09-15-trip-inbox-design.md
- [x] 2026-09-15 — **One timeline for a trip** (`851fb62`): trip_items replaces trip_segments + segment_candidates: flights, stays, activities and transport in one date-ordered list, positions recomputed on every write; add_trip_item from chat; finalise_trip sums fixed costs. Spec: docs/superpowers/specs/2026-09-15-trip-timeline-design.md
- [x] 2026-09-14 — **Debug trace behind every answer** (`b3708e0`): `/debug on|off` in the web chat (admins only, per account). Every run records a trace: each tool call with args, duration, status and result, nested flight-desk calls, run-level events. A Trace button under each Scout turn opens the panel; live rows stream during a run. Spec: `docs/superpowers/specs/2026-09-13-debug-trace-design.md`
- [x] 2026-09-08 — **Crawlable front door** (`532f30e`): robots.txt, sitemap.xml, meta description and Open Graph tags on the landing page, www redirected to the apex. Google still showed Porkbun's parking page; www needs an A record at the DNS provider first.
- [x] 2026-09-08 — **Flight agent as a tool** (`39e5d78`): `ask_flights`, a specialist rig agent with the flight prompt and every flight and trip tool, called by the main agent with a brief; findings plus Rust-generated guidance come back, the main prompt lost its flight half, `FLIGHT_MODEL` picks its model, and the stall guard reads a pulse the nested run keeps alive
- [x] 2026-09-05 — **Two things a web-only user ran into** (`de08a65`): the page's form token now lives as long as the session, so a phone left on the chat keeps sending instead of failing every POST after 15 minutes; `/stat` names an account by its email when it has no Telegram name
- [x] 2026-09-05 — **Web messages count in `/stat` and toward the daily cap** (`0157c4f`): one `log_request` in the web send path after the ownership check; a refused message still counts for nothing
- [x] 2026-09-05 — **The whole thread on the page, and titles you can read** (`3a6a74e`): the message table is the full log and only the model's window is trimmed from it; rig's final response excludes the input history, so every save had been dropping earlier turns — follow-ups now really have context; pinned threads' logs are bounded at 2000 rows; sidebar titles are the biggest thing on their row, two lines, tools beneath on the current row
- [x] 2026-09-05 — **Four cards from the board** (`80b1ad9`): the `/chat/reset` route is gone; a 422 takes its "You" bubble off the screen; expiry spares a thread with a run in flight, and a Telegram continuation that finds its thread gone starts a fresh one; the expiry countdown keeps ticking on an open tab
- [x] 2026-09-05 — **Deployed `df4c178`** to the k3s node; migration steps 7 and 8 ran on the production database on first boot
- [x] 2026-09-05 — **Threads in the browser** (`df4c178`): sidebar, switch, rename, model-suggested rename, pin, delete; 48-hour expiry; titles from the person's words; the mirror queues the thread that ran; `MINIMAX_BASE_URL` configurable and every test off the network
- [x] 2026-09-05 — **Retry with backoff** on every paid provider (`a159bf7`): 429/5xx/refused connection, Retry-After honoured to 10 s, timeouts deliberately not retried
- [x] 2026-09-05 — **Cap on runs in flight** (`5319ebd`): eight slots, queued notice, "try again in a minute" after two minutes
- [x] 2026-09-05 — **Poisoned store lock no longer takes every call down** (`d6e8a04`)
- [x] 2026-09-05 — **Web-admitted members reach the Telegram gate without a restart** (`3d2c7b2`)
