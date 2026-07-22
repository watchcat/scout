# Scout — Product-Search Telegram Bot: Design

Date: 2026-07-22
Status: Approved

## Purpose

A Telegram bot backed by an AI agent that searches the web for products,
compares options, and replies with summaries and links. The user completes
purchases themselves — the bot never buys anything.

## Scope

**In scope (v1):**
- Telegram bot (teloxide, long polling) restricted to an allowlist of user IDs
- AI agent (rig) powered by MiniMax M3 via its OpenAI-compatible API
- Two native Rust tools calling Kagi's HTTP API: web search and page summarizer
- Multi-turn conversations: per-chat history so the user can refine searches
  ("only hot-swappable", "what about #2's shipping?")
- Purchase memory in DuckDB: the user tells the bot what they bought, the
  agent records it, and can later answer "where did I buy X last time?"

**Out of scope (v1), doors left open:**
- Purchasing / checkout automation of any kind
- Price tracking, alerts, scheduled monitoring
- Persistent conversation history (chat history is in-memory, lost on
  restart; only purchases persist, in DuckDB)
- MCP servers (rig's tool abstraction allows adding rmcp-based MCP clients
  later without restructuring)
- Public access, rate limiting, billing controls

## Architecture

One Rust binary, no external services beyond the three APIs.

| Module | Responsibility |
|---|---|
| `config.rs` | Load and validate `.env` values via `dotenvy`; parse `ALLOWED_TELEGRAM_USER_IDS` (comma-separated) into a set |
| `bot.rs` | Teloxide dispatcher: allowlist filter, `/start`, `/help`, `/reset` commands, main text-message handler |
| `agent.rs` | Build the rig agent: MiniMax M3 through rig's OpenAI-compatible provider with base URL `https://api.minimax.io/v1`, model `minimax-m3`; product-research system prompt; tool registration |
| `tools/kagi.rs` | `kagi_search` and `kagi_summarize` implementing rig's `Tool` trait as `reqwest` calls against the Kagi API |
| `tools/purchases.rs` | `record_purchase` and `query_purchases` implementing rig's `Tool` trait on top of `store.rs` |
| `store.rs` | DuckDB access: open/create the database file, run migrations, insert and query purchases |

### Configuration (`.env`)

- `TELEGRAM_BOT_TOKEN` — from @BotFather
- `ALLOWED_TELEGRAM_USER_IDS` — comma-separated numeric IDs
- `MINIMAX_API_KEY`
- `KAGI_API_KEY`
- `SCOUT_DB_PATH` — optional; path to the DuckDB file, default `scout.duckdb`
  in the working directory

Missing/malformed values fail fast at startup with a clear message.

### Conversation state

- `DashMap<ChatId, Vec<Message>>`, capped at the last 20 messages per chat to
  bound token spend
- `/reset` clears the calling chat's history
- In-memory only; restart loses context (acceptable for a personal bot)

### Purchase memory (DuckDB)

Purchases persist in a single DuckDB file so the bot can answer "where did I
buy X last time?" across restarts.

Schema (created on startup if missing):

```sql
CREATE TABLE IF NOT EXISTS purchases (
    id          INTEGER PRIMARY KEY,      -- from a sequence
    user_id     BIGINT NOT NULL,          -- Telegram user ID
    item        TEXT NOT NULL,            -- what was bought
    store       TEXT NOT NULL,            -- where (site/shop name)
    url         TEXT,                     -- product link, if known
    price       DOUBLE,                   -- amount paid, if known
    currency    TEXT,                     -- e.g. USD, PLN
    notes       TEXT,
    purchased_at DATE,                    -- when bought (may be in the past)
    recorded_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
```

Agent tools on top of it:

- `record_purchase(item, store, url?, price?, currency?, notes?,
  purchased_at?)` — called when the user says they bought something; the
  agent confirms what it recorded in its reply
- `query_purchases(search_term?, limit?)` — case-insensitive substring match
  on item/store/notes, most recent first; used to answer "where did I get X?"

Purchases are scoped per Telegram user ID — each allowlisted user sees only
their own history. DuckDB is accessed through a `tokio` blocking-task wrapper
since its Rust crate is synchronous; a single connection behind a mutex is
plenty at this scale.

### System prompt (agent behavior)

The agent acts as a product-research assistant: searches with `kagi_search`,
optionally pulls page details with `kagi_summarize`, compares options, always
cites prices and links, and asks the user for missing criteria (budget,
region, must-have features) instead of guessing. When the user mentions
having bought something, it records it with `record_purchase`; when asked
about past purchases (or when past purchases are relevant to a new search),
it checks `query_purchases`.

## Data flow

1. Message arrives → allowlist check; messages from unknown users are
   silently ignored
2. Bot sends `typing…` chat action
3. Chat history is loaded and passed with the new message to the rig agent
4. Agent runs its tool loop (search → maybe summarize → maybe search again),
   capped at 5 tool-call rounds per request
5. Final answer is split into ≤4096-char chunks (never splitting inside a
   link) and sent to Telegram
6. User message + agent reply are appended to history (trimming to the cap)

## Error handling

- Kagi or MiniMax API failure → short apologetic Telegram reply; full error
  with context to `tracing` logs
- Timeouts on all outbound HTTP calls (Kagi, MiniMax)
- Tool-loop iteration cap so a confused agent cannot burn API credits
- Telegram send failures are logged and retried once

## Testing

- Unit tests: config parsing (valid/missing/malformed), allowlist logic,
  message chunking (length boundaries, link preservation), Kagi
  request/response serialization against `wiremock`-mocked HTTP
- Store tests: insert/query round-trips against a temp-file DuckDB,
  including per-user scoping and substring matching
- End-to-end with the real LLM loop: manual, once real keys are in `.env`

## Key dependencies

- `teloxide` — Telegram bot framework
- `rig-core` — agent framework (OpenAI-compatible provider)
- `reqwest` — HTTP client for Kagi tools
- `duckdb` — purchase memory (bundled feature, so no system DuckDB install)
- `tokio` — async runtime
- `dashmap`, `dotenvy`, `tracing`, `tracing-subscriber`, `serde`,
  `thiserror`/`anyhow`
- Dev: `wiremock`
