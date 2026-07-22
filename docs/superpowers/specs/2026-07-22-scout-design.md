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

**Out of scope (v1), doors left open:**
- Purchasing / checkout automation of any kind
- Price tracking, alerts, scheduled monitoring
- Persistent storage (history is in-memory, lost on restart)
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

### Configuration (`.env`)

- `TELEGRAM_BOT_TOKEN` — from @BotFather
- `ALLOWED_TELEGRAM_USER_IDS` — comma-separated numeric IDs
- `MINIMAX_API_KEY`
- `KAGI_API_KEY`

Missing/malformed values fail fast at startup with a clear message.

### Conversation state

- `DashMap<ChatId, Vec<Message>>`, capped at the last 20 messages per chat to
  bound token spend
- `/reset` clears the calling chat's history
- In-memory only; restart loses context (acceptable for a personal bot)

### System prompt (agent behavior)

The agent acts as a product-research assistant: searches with `kagi_search`,
optionally pulls page details with `kagi_summarize`, compares options, always
cites prices and links, and asks the user for missing criteria (budget,
region, must-have features) instead of guessing.

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
- End-to-end with the real LLM loop: manual, once real keys are in `.env`

## Key dependencies

- `teloxide` — Telegram bot framework
- `rig-core` — agent framework (OpenAI-compatible provider)
- `reqwest` — HTTP client for Kagi tools
- `tokio` — async runtime
- `dashmap`, `dotenvy`, `tracing`, `tracing-subscriber`, `serde`,
  `thiserror`/`anyhow`
- Dev: `wiremock`
