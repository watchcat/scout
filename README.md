# Scout

Telegram bot + AI agent that researches products online, remembers what you
bought, and reminds you when it's time to reorder. It never buys anything —
you get links and comparisons, purchasing stays in your hands.

Built with [teloxide](https://github.com/teloxide/teloxide),
[rig](https://rig.rs) (MiniMax M3 via its OpenAI-compatible API),
[Kagi Search API (v1)](https://help.kagi.com/kagi/api/search.html), and
DuckDB for purchase memory.

## Features

- Conversational product search with follow-up refinement; opens product
  pages to extract direct links and prices instead of listing-page URLs
- Photo search: send a product photo, confirm/edit the drafted query
  (one-tap copy button), search
- Second-hand search across eBay/Marktplaats/Vinted (per-user configurable
  list), queried in parallel; dead listings (404/410) are filtered out
- Live eBay data via the official Browse API when `EBAY_CLIENT_ID`/`SECRET`
  are set: real prices, condition and availability instead of search snippets
- Live Marktplaats data via the site's public search JSON (no keys needed):
  prices with bid-vs-fixed labeling, seller city, direct listing links
- Purchase memory: "where did I buy X last time?" — plus 👍 a suggestion to
  save it as a purchase
- Per-user profile memory (delivery country, sizes, preferred marketplaces)
  injected into every request, so the bot stops re-asking
- Reorder reminders for periodic purchases, delivered in Telegram
- Sessions auto-reset after 10 idle minutes, with LLM-checked restore when
  you continue the same topic

Note: page summarization (Kagi Universal Summarizer) is temporarily removed —
Kagi's v1 API doesn't offer it yet; it returns when they ship it.

## Setup

1. Create a bot with [@BotFather](https://t.me/BotFather), copy the token.
2. Get your numeric Telegram user id from [@userinfobot](https://t.me/userinfobot).
3. Get a [Kagi API key](https://kagi.com/settings?p=api) (paid) and a
   [MiniMax API key](https://www.minimax.io).
4. Copy `.env.example` to `.env` and fill in the values.
5. `cargo run` (first build takes a while — DuckDB is compiled in).

## Configuration

| Variable | Required | Default | Purpose |
|---|---|---|---|
| `TELEGRAM_BOT_TOKEN` | yes | — | bot token from @BotFather |
| `ALLOWED_TELEGRAM_USER_IDS` | yes | — | comma-separated numeric ids |
| `MINIMAX_API_KEY` | yes | — | LLM |
| `KAGI_API_KEY` | yes | — | search |
| `SCOUT_DB_PATH` | no | `scout.duckdb` | DuckDB file |
| `SECONDHAND_SITES` | no | `ebay.com,marktplaats.nl,vinted.com` | second-hand domains |
| `EBAY_CLIENT_ID` + `EBAY_CLIENT_SECRET` | no | — | eBay Browse API: live prices/availability for eBay results |
| `EBAY_MARKETPLACE` | no | `EBAY_NL` | eBay marketplace id |

## Running with Docker

```
docker compose up -d --build
```

Reads keys from `.env` at runtime (never baked into the image). Purchase
memory lives in the `scout-data` volume, so it survives rebuilds; the
container restarts automatically unless stopped (`docker compose down`).
Logs: `docker compose logs -f`.

## Development

`cargo test` — unit tests (HTTP mocked with wiremock, DuckDB on temp files).
`RUST_LOG=debug cargo run` — verbose logs.
