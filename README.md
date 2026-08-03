# Scout

**A Telegram shopping researcher that doesn't make things up.**

Scout finds products, compares real prices, and remembers what you bought. It
never buys anything — you get verified links and honest comparisons, and the
purchase stays in your hands.

Built in Rust with [teloxide](https://github.com/teloxide/teloxide),
[rig](https://rig.rs), DuckDB, and a deep suspicion of what language models
say about prices.

```
You:   найди самый дешёвый Vanish Oxi Action colour

Scout: 💭 checking your purchase history…
       🔎 searching in 3 languages: vanish oxi action colour
       📄 opening action.com
       🧮 comparing 6 offers per kilo

       Cheapest one-off
       EUR 3.98 — 600 g, Action (EUR 6.63/kg), in stock
       https://www.action.com/nl-nl/p/2577875/vanish-oxi-action-…

       Best per unit
       EUR 13.80 — 1.5 kg, bol.com (EUR 9.20/kg), sold by MYSCO
       https://www.bol.com/nl/nl/p/vanish-oxi-action-poeder-…

       Buying the big pack does not save here — the 600 g at Action is
       cheaper per kilo than any bulk option I found.

       Prices are item-only; neither shop states shipping before checkout.
```

---

## Why this exists

Ask any chat model to find you the cheapest detergent and it will answer
confidently. It will also invent product URLs, quote a price from a
recommendations carousel, compare a 3-pack against a single bottle, and
recommend something the shop stopped selling last March.

Scout is what's left after fixing each of those, one production failure at a
time. Every row below was a real bug reported by a real user, and every fix
is pinned by a test.

| What went wrong | What Scout does now |
|---|---|
| Four Amazon links, all fabricated. An invented `/dp/<ASIN>` looks perfectly real and always 404s. | Every link in every reply is probed before sending. Dead ones trigger a correction turn, then get struck from the text. |
| Quoted €12.99 for a bottle the page priced at €13.80 — the €12.99 came from a "related items" strip. | Prices are read from the page's **structured data** (JSON-LD, microdata, or the shop's embedded app state) for the exact URL opened — never from flattened page text. |
| **€640 for a €6.48 bottle of cola.** The shop published two JSON-LD blocks: `"6.48"` and `"648"` (cents), and we picked the wrong one. | Relative URLs resolve properly, a price written with decimals beats a bare integer, and anything exactly 100× the page's OpenGraph price is corrected. |
| Recommended a bol.com listing marked *Niet leverbaar*. The page returns HTTP 200, so nothing looked wrong. | Stock status comes from schema.org markup. Prose is ignored — that page says "Niet leverbaar" once and "In winkelwagen" seven times, all from its carousel. |
| Compared a €15 3-pack against a €35 single and called the 3-pack cheaper. | A `compare_prices` tool does the arithmetic **in Rust**: landed cost (item + shipping), price per unit, exact ranking. The model reports numbers, it doesn't compute them. |
| A user was left staring at a half-written progress message forever. | Every run is bounded — stalled stream, total runtime, wrap-up. Running out of steps produces a partial answer from what was found, never silence. |
| Sticker price ranked a €15.17 listing above a €35.25 one; shipping was €16.98 vs €2.21. | Comparisons are on **landed cost**. Offers that don't state shipping still compete, but their prices are labelled item-only. |

The theme: **the model decides what to look for, Rust decides what's true.**

---

## What it does

**Finds things**
- Conversational search with follow-ups — "only under €20", "what about second-hand?"
- Two search engines (Kagi + Perplexity) merged and deduplicated. On the same
  Dutch query their results overlapped by **2 URLs out of 20** — each finds
  small shops the other misses
- Searches in your local languages as well as English, all in one call.
  "laundry detergent" finds nothing on a Dutch shop; "wasmiddel" does
- Favourite shops, scoped by category — "always check 123schoon.nl for
  cleaning products". Small shops never rank organically, even under their
  product's exact name; a `site:` query is the only way to reach them
- Send a **photo**, get a drafted search description to edit and confirm

**Verifies things**
- Opens product pages and extracts the real link, price, seller and stock
- Pages that block plain HTTP get re-opened in **headless Chrome**, which
  clears the challenge shops like action.com put in front of them
- Live APIs where they exist: eBay Browse, bol.com Catalog, Marktplaats —
  current prices with no scraping at all
- Second-hand search across eBay / Marktplaats / Vinted in parallel, with
  sold and deleted listings filtered out

**Remembers things**
- Purchase history: *"where did I buy this last time?"* — react 👍 to a
  suggestion and Scout offers to save it
- Your profile: delivery country, sizes, preferred marketplaces, languages.
  Injected into every request so it stops re-asking
- Reorder reminders for things you buy periodically, delivered in Telegram
- `/stat` for per-user usage with a text bar chart

**Behaves itself**
- Streams progress live — which tool is running and on what — then the answer
  as it's written, all in one edited message
- Sessions reset after 10 idle minutes, with an LLM check that restores
  context when you're clearly continuing the same topic
- Hard caps on searches, page opens and turns per request, so no single
  question can run away with your API budget

---

## Quick start

```bash
git clone https://github.com/watchcat/scout && cd scout
cp .env.example .env    # fill in the four required keys
docker compose up -d --build
```

You need four things:

1. A bot token from [@BotFather](https://t.me/BotFather)
2. Your numeric Telegram id from [@userinfobot](https://t.me/userinfobot) —
   Scout only answers people on the allowlist
3. A [MiniMax API key](https://www.minimax.io) (the LLM)
4. A [Kagi API key](https://kagi.com/settings?p=api) (search, paid)

Everything else is optional and makes Scout better as you add it.

Prefer to run it directly? `cargo run` — the first build takes a while, DuckDB
compiles from source.

### Configuration

| Variable | Required | Default | Purpose |
|---|---|---|---|
| `TELEGRAM_BOT_TOKEN` | **yes** | — | bot token from @BotFather |
| `ALLOWED_TELEGRAM_USER_IDS` | **yes** | — | comma-separated numeric ids |
| `MINIMAX_API_KEY` | **yes** | — | the LLM |
| `KAGI_API_KEY` | **yes** | — | search: small-retailer coverage, `site:` scoping |
| `PERPLEXITY_API_KEY` | no | — | second engine, merged with Kagi; carries the multi-language fan-out cheaply |
| `EBAY_CLIENT_ID` + `EBAY_CLIENT_SECRET` | no | — | eBay Browse API: live prices, condition, shipping |
| `EBAY_MARKETPLACE` | no | `EBAY_NL` | eBay marketplace id |
| `BOL_CLIENT_ID` + `BOL_CLIENT_SECRET` | no | — | bol.com Catalog API (affiliate account) |
| `BOL_COUNTRY` | no | `NL` | `NL` or `BE` |
| `SECONDHAND_SITES` | no | `ebay.com,marktplaats.nl,vinted.com` | second-hand domains |
| `SCOUT_CHROME` | no | auto-detected | Chrome/Chromium for the headless fallback |
| `SCOUT_DB_PATH` | no | `scout.duckdb` | DuckDB file |

Keys are read from `.env` at runtime and never baked into the image. Memory
lives in the `scout-data` volume and survives rebuilds.

---

## How it works

```
Telegram ──► bot.rs ──► rig agent (MiniMax M3) ──► 12 tools
                │                                    │
                │  streams progress + answer         ├─ search_web        Kagi + Perplexity, merged
                │  back into one edited message      ├─ search_secondhand eBay / Marktplaats / Vinted
                │                                    ├─ search_bol        live bol.com catalogue
                ▼                                    ├─ fetch_page        + headless-Chrome fallback
        link verification                            ├─ compare_prices    deterministic, in Rust
        (nothing dead ships)                         ├─ query_purchases   ─┐
                                                     ├─ record_purchase    ├─ DuckDB
                                                     ├─ remember_fact      │
                                                     ├─ forget_fact       ─┘
                                                     └─ reminders (create/list/cancel)
```

The agent chooses tools; the tools enforce the rules. Page budgets, search
budgets, dead-link probes, price extraction and the price maths all live in
Rust, where they can be tested — `cargo test` runs **164 tests** with HTTP
mocked via wiremock and DuckDB on temp files. No network, no API keys, no
flakiness.

Roughly 8,500 lines of Rust across a dozen focused modules.

---

## Honest limitations

- **Curated marketplaces.** eBay, bol.com, Marktplaats and Vinted are wired in
  deliberately; everything else arrives through general web search. Excellent
  for NL/EU, thinner elsewhere.
- **Not every wall falls.** Headless Chrome clears Cloudflare on some shops and
  not others. When a page can't be verified, Scout says so rather than
  guessing.
- **A shop that lies convincingly wins.** If a page publishes only a
  bare-integer cents price with nothing to cross-check, Scout reports that
  number instead of inventing a division.
- **Allowlist only.** This is built as a personal/household bot. There's no
  multi-tenant isolation beyond per-user memory scoping.
- **Costs real money.** Kagi bills per query and MiniMax per token. Budgets are
  capped per request (15 search queries, 5 page opens, 16 model turns) — but
  it's not free.

---

## Development

```bash
cargo test                  # 164 tests, no network needed
cargo clippy --all-targets  # clean
RUST_LOG=debug cargo run    # verbose logs
docker compose logs -f      # what the bot is doing right now
```

Docker builds use BuildKit cache mounts: a code change rebuilds in **~20s**, a
dependency change in ~5s, because Cargo's registry and `target/` survive layer
invalidation. Only a cold cache pays the full compile.

---

## License

[MIT](LICENSE) — do what you like with it.
