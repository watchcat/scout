# Scout — "Cheapest X" Requests: Design

Date: 2026-07-31
Status: Approved

## Purpose

"Find the cheapest X" is not one question. Today the agent answers it with a
single list sorted by sticker price, which is wrong in three common ways:

- **Pack size is ignored.** A 3-pack at €15 and a single blade at €35 sort
  as "€15 wins" — per blade it is €5 vs €35, a different conclusion and a
  different magnitude.
- **Shipping is ignored.** Live eBay data for "philips oneblade blades":
  a €15.17 listing carries €16.98 shipping (€32.15 landed) while a €35.25
  listing ships for €2.21 (€37.46 landed). Sticker order and landed order
  disagree.
- **Arithmetic is the model's.** Per-unit prices computed inside the LLM
  drift between replies for the same offers.

This design gives cheapest-requests two explicit answers — cheapest one-off
and best price per unit — both on a landed-cost basis, with the arithmetic
moved out of the model into Rust.

## Scope

**In scope:**
- A `compare_prices` tool: deterministic landed-cost and per-unit ranking
- Shipping cost from the eBay Browse API (already in the response, discarded
  today)
- A preamble protocol for cheapest/best-price requests
- Explicit handling of offers whose shipping is unknown

**Out of scope (deliberately):**
- A dedicated "cheapest second-hand" tier — second-hand offers take part in
  the same comparison when they are found, but get no separate headline
- Comparing today's price against what the user paid before (purchase memory
  stays as it is)
- Currency conversion — a comparison is single-currency
- Quantity discounts from buying the same listing N times

## Answer shape

A cheapest-request produces two headline picks and a short tail:

```
Cheapest one-off
€37.46 delivered — 1 blade, eBay (€35.25 + €2.21 shipping)
https://...

Best per unit
€32.15 delivered — 3-pack, eBay (€15.17 + €16.98 shipping)
= €10.72 per blade, 71% less than buying one
https://...

Also seen: bol.com €12.95/blade delivered (shipping unknown), ...
```

"Bulk" means a single listing containing N units (a multipack or bundle),
not buying the same listing repeatedly. When no multipack exists, or when
its per-unit price does not beat the single, the reply says so and gives one
headline pick rather than padding to two.

## `compare_prices` tool

**Input**

```json
{
  "unit_name": "blade",
  "offers": [
    {
      "title": "3 Pack of Philips Genuine OneBlade Blades",
      "url": "https://...",
      "shop": "ebay.com",
      "price": 15.17,
      "currency": "EUR",
      "units": 3,
      "shipping": 16.98,
      "condition": "new",
      "note": "ships from UK"
    }
  ]
}
```

`unit_name`, `title`, `url`, `price`, `currency` and `units` are required;
`shipping`, `condition` and `note` are optional. Every offer in one call must
share a currency and count in the same unit — the tool rejects the call
otherwise instead of silently comparing apples to grams. The model is told to
call it once per comparison with every candidate, not once per offer.

**Computation**

- `landed = price + shipping` (shipping absent → `landed = price`,
  `shipping_known: false`)
- `per_unit = landed / units`
- rows sorted by `per_unit` ascending
- `best_single` = lowest `landed` among offers with the smallest `units`
  value in the set (usually 1)
- `best_per_unit` = lowest `per_unit` overall; when it is the same offer as
  `best_single`, the output says there is no bulk advantage
- money rounded to 2 decimals, savings to whole percent, both in the tool

**Output**

```json
{
  "unit_name": "blade",
  "currency": "EUR",
  "best_single": { "...row..." },
  "best_per_unit": { "...row...", "saving_vs_single_pct": 71 },
  "rows": [ { "title", "url", "shop", "units", "price", "shipping",
              "landed", "per_unit", "shipping_known", "condition", "note" } ],
  "notes": ["2 offers have unknown shipping and were ranked on item price only"]
}
```

**Validation** (rejected with a message telling the model how to fix the
call): empty offer list, mixed currencies, `units` < 1 or non-integer,
non-finite or negative `price`/`shipping`, missing `url`.

## Unknown shipping is never zero

Shipping is known when the eBay Browse API reports it, or when a page opened
with `fetch_page` states it. Otherwise it is absent, and:

- the offer keeps `shipping_known: false` and is ranked on item price alone
- it can never take a headline pick from a fully-known offer — it appears
  only in the tail, labelled "shipping unknown"

Without this rule "cheapest" systematically favours shops that hide shipping
until checkout.

## Data plumbing

`EbayItem` gains a `shipping` field, read from
`shippingOptions[0].shippingCost.value` (the marketplace-currency figure,
already converted by eBay). The snippet the model sees becomes
`"15.17 EUR + 16.98 shipping · New · live eBay listing"`, so the value
survives into the `compare_prices` call.

Marktplaats and Kagi results carry no shipping figure; those offers are
passed with shipping absent unless a fetched page states it.

## Preamble protocol

A rule fires on cheapest / best-price / "how cheap can I get" requests:

1. Search as usual (`kagi_search`, `search_secondhand`, ≤3 `fetch_page` —
   the existing budget is unchanged).
2. While reading results, record each offer's pack size and shipping cost
   when stated. Never invent either; omit what is not stated.
3. Call `compare_prices` once with every candidate offer.
4. Answer with the two headline picks and at most three runners-up, taking
   every number from the tool's output verbatim.
5. State the assumed pack size when it was inferred rather than stated, so a
   wrong reading is visible to the user.
6. Say plainly when no bulk option beats the single, and which offers have
   unknown shipping.

## Failure modes

- **Wrong pack size.** The tool cannot detect that `units` is wrong; it can
  only be consistent about it. Mitigated by rule 5 — assumptions are stated
  in the reply.
- **Shipping quoted for the wrong destination.** eBay's figure is for the
  configured marketplace; pages fetched directly may show a domestic rate.
  Offers keep their `note` field for this kind of caveat.
- **Model skips the tool.** Nothing enforces the call; the preamble rule is
  the only mechanism. If this proves unreliable in use, the fallback is to
  reject replies containing per-unit claims without a preceding tool call —
  not part of this change.

## Testing

- `compare_prices`: bulk wins; bulk loses (no-advantage path); unknown
  shipping held out of headline picks; mixed-currency rejection; `units` < 1
  and negative price rejection; per-unit tie between single and multipack;
  rounding of savings percentage.
- eBay client: shipping parsed into `EbayItem` and rendered in the snippet;
  listing without `shippingOptions` yields absent shipping, not zero.
