# Board

What is being worked on, what is next, and what is waiting. One line per
card; the design docs and plans under `docs/superpowers/` carry the detail.
Move a card by moving its line. Add the date when a card lands in **Done**.

## In progress

- [ ] Deploy `df4c178` (threads in the browser) to the k3s node — first boot runs migration steps 7 and 8 on the production database

## Next

- [ ] **Poller liveness** — record the last successful `getUpdates`; fail `/healthz` when it is stale so k8s restarts a bot Telegram has gone quiet on (24 restarts in 20 minutes happened once with nothing to catch it)
- [ ] **Retention for the rest** — `outbox` never deletes sent rows, `request_log` never prunes; two DELETEs in `run_maintenance` next to the thread expiry
- [ ] **CI** — `.github/workflows`: `cargo test`, `cargo clippy -D warnings`, `cargo audit`, `node --test 'crates/scout-web/src/*.test.mjs'`, cached with `Swatinem/rust-cache`
- [ ] **Timeout on `continues_previous`** — same shape as `TITLE_BUDGET` on `title_for`; this one runs on every Telegram message after a 10-minute gap and has no bound

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

### Hygiene
- [ ] **Duplicate dependency trees** — reqwest 0.12 + 0.13, rand 0.8/0.9/0.10, sha2 0.10/0.11; bumping reqwest to 0.13 (what rig uses) drops one TLS stack
- [ ] **`proc-macro-error2` future-incompat** — transitive; check for an updated upstream
- [ ] **Backup restore drill** — the restic repository has never been restored from; one rehearsal on a scratch node
- [ ] **Concurrency doc** — `docs/2026-08-08_225900-scout-100-concurrent-users.md` is in Russian while everything else is English, and most of it has shipped; rewrite as a status page or retire it
- [ ] **`target/` is 49 GB** locally — `cargo sweep` or a periodic clean

### Threads follow-ups (from the reviews)
- [ ] Drop the dead `conversations.pending_draft` column — nothing reads it; the live draft is in the Telegram adapter
- [ ] Remove the `/chat/reset` alias — no client calls it since the sidebar posts `/chat/threads`
- [ ] On a 422 the "You" bubble stays alongside the restored composer text
- [ ] `expire_conversations` has no "not currently running" guard — a thread crossing 48h mid-run is deleted; the answer is delivered, the orphans are swept next hour
- [ ] "expires in Nh" is computed at render time only; a tab left open shows a stale number until it regains focus

## Done

- [x] 2026-09-05 — **Threads in the browser** (`df4c178`): sidebar, switch, rename, model-suggested rename, pin, delete; 48-hour expiry; titles from the person's words; the mirror queues the thread that ran; `MINIMAX_BASE_URL` configurable and every test off the network
- [x] 2026-09-05 — **Retry with backoff** on every paid provider (`a159bf7`): 429/5xx/refused connection, Retry-After honoured to 10 s, timeouts deliberately not retried
- [x] 2026-09-05 — **Cap on runs in flight** (`5319ebd`): eight slots, queued notice, "try again in a minute" after two minutes
- [x] 2026-09-05 — **Poisoned store lock no longer takes every call down** (`d6e8a04`)
- [x] 2026-09-05 — **Web-admitted members reach the Telegram gate without a restart** (`3d2c7b2`)
