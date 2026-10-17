# Trying a second model on the side calls

The board card: GPT-5.6 Luna is about 30% cheaper than MiniMax M3 on
list price, but reasoning tokens may eat the difference, and tools plus
reasoning need the Responses API, which Scout does not speak. So the
trial starts where neither is at stake: the five tool-less side calls.

## What is in place

`SIDE_LLM_MODEL` sends exactly these to a second model, nothing else:

| call           | what it is                                   | budget |
|----------------|----------------------------------------------|-------:|
| `title`        | a name for a thread                          |   30 s |
| `continuation` | CONTINUE or NEW after ten quiet minutes      |   30 s |
| `extract`      | a mailed booking read into JSON              |   60 s |
| `photo`        | a product photo into a search query          |   60 s |
| `page`         | a drawn or scanned PDF page into its text    |   60 s |

`SIDE_LLM_BASE_URL` and `SIDE_LLM_API_KEY` default to the main
provider's, so a cheaper model at MiniMax is one variable and OpenAI is
three. The main agent, the flight desk and the wrap-up stay where they
are. Every side call is counted on `/metrics` by call and model:

- `scout_side_calls_total{call, model, outcome}` — `ok`, `empty` (the
  model answered with reasoning and nothing else), `failed`, `timeout`;
- `scout_side_call_tokens_total{call, model, kind}` — `input`, `cached`,
  `output`, and `reasoning` on its own, which is the number the card is
  worried about;
- `scout_side_call_ms_total{call, model}` — divide by the calls for a mean.

## How to run it

1. In `.env`, with the exact model id the provider lists (do not guess
   it; `GET /v1/models` with the key says):

   ```
   SIDE_LLM_MODEL=<model id>
   SIDE_LLM_BASE_URL=https://api.openai.com/v1
   SIDE_LLM_API_KEY=<key>
   ```

2. `scripts/deploy-k3s.sh` — the secret is rebuilt from `.env`. The pod
   log says `the side calls go to a second model` on start.

3. After a week, `scripts/metrics.sh scout_side_call`. The counts restart
   with the pod, so read them before the next deploy, or from a scrape.

## What to compare

- **Cost.** Per model: `(input − cached) × price_in + cached ×
  price_cached + output × price_out`, with `reasoning` inside `output` —
  a model that thinks for a one-word verdict pays for the thinking.
  `cached` says whether the provider's cache earns anything on these
  short prompts (MiniMax's does not; `docs/2026-10-17-prompt-cache.md`).
- **Time.** `ms_total / calls` per call and model. The continuation check
  sits in front of every answer after a quiet gap; a slower model there
  is a slower bot.
- **Failures.** `ok` means answered in budget with something said.
  `empty` is the case to watch on a reasoning model: it thought and said
  nothing, which paid for the thinking and failed the call. `failed` is
  any error reaching the client — a refusal, a model that will not take
  an image on the chat endpoint (`photo`, `page`), unreadable JSON;
  `timeout` is the budget.
- **Quality**, by hand, since nothing counts it: thread titles in the
  sidebar; whether a conversation resumed after a break kept or lost its
  context; the pending rows a mailed booking produced — a wrong date is
  a refused row, visible under Other mail; a photo's query in the chat.

Then decide per call: the five are independent settings in all but name,
and splitting them is a small change if one model wins some and loses
others.
