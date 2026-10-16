# Search fan-out: what the traces say

The board card asked how often the later Kagi queries in a request add a
result, since Kagi bills per query and a request may spend up to 15. This
is the reading of 2026-10-16, from `scripts/search-fanout.py` run over the
nightly backup: 143 runs between 2026-09-13 and 2026-10-15, 103 of them
with traces, 65 with at least one search. Small numbers; read them as
shape, not precision.

## How a search spends queries

One `search_web` call sends the user's query to Kagi and, when the model
supplies translations, up to two more, concurrently. Each returns 10 hits
alone or 6 when fanned out; Perplexity runs beside them, unbilled. The
lists are merged by URL — the user's query first, then each translation's
new URLs, then Perplexity's — and cut at 12. `search_secondhand` draws on
the same 15-query allowance.

## What was measured

- **Translations are rare.** 149 of 175 calls (85%) asked one query; 14
  asked two and 12 asked three. Every call that succeeded returned exactly
  12 results: the cap always bites, even alone (10 from Kagi plus at least
  two new ones from Perplexity).
- **A run spends three queries, not fifteen.** Median 3 Kagi queries a
  run, mean 3.4, at most 12. No run reached the allowance; no search was
  refused for want of it. 47 of 65 runs spent three or fewer; 9 spent more
  than five. Across the window, 221 queries in all — at Kagi's list price
  of $0.025 a query, about $5.50 a month.
- **Later searches still find new pages.** The second to fifth searches
  in a run return 89–98% URLs no earlier search in that run had; only
  from the sixth on does that fall, to 79%. The model asks different
  things, not the same thing again.
- **Later searches are opened less.** Of 119 pages the model went on to
  fetch, 91 came from a search result (the rest it typed from what it
  knew). Per call, the first search in a run led to 0.85 opened pages,
  the second 0.51, the third 0.30, the fourth 0.33, the fifth 0.20.
- **Translated calls are opened more.** A single-query call led to 0.43
  opened pages; a two- or three-query call to about 1.0. In fanned calls
  the opened pages split evenly between the first six slots (the user's
  query) and slots 7–12 (translations and Perplexity) — 14 to 13.
- **The third query is paid for and thrown away.** With three queries
  Kagi returns 18 hits for 12 slots; the first two languages fill the cap
  unless they overlap, so the third's results are cut before the model
  sees them. The merged list does not record which query a hit came from,
  so the traces cannot say how often that happened — only that the shape
  guarantees it.

## What follows

- The allowance stays at 15: nobody reaches it, and lowering it to the
  level that would save money (six) would refuse a search in one run in
  seven to save about a tenth of the spend.
- Each query's share of the merged list is now sized so all of them fit:
  10 alone, 6 for two, 4 for three. A billed query's results reach the
  model.
- `/metrics` carries the exact tally from here on —
  `scout_search_results_total{source, outcome}` counts, per Kagi query
  position and for Perplexity, the hits kept, the duplicates of an earlier
  list, and the ones cut by the cap; `scout_search_calls_total{queries}`
  counts calls by how many Kagi queries they spent. The next reading can say
  what the traces could not.
- The fan-out is not where the money goes. If Kagi is still the dominant
  cost line, it is the price of the first three queries of every run, and
  the lever is Perplexity's share of the list, not the cap.
