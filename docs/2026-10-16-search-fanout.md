# Search fan-out: what the traces say

The board card asked how often the later Kagi queries in a request add a
result, since Kagi bills per query and a request may spend up to 15. This
is the reading of 2026-10-16, from `scripts/search-fanout.py` run over the
nightly backup: 143 runs between 2026-09-13 and 2026-10-15, 103 of them
with traces, 65 with at least one search. Small numbers; read them as
shape, not precision.

## How a search spends queries

One `search_web` call sends the user's query to Kagi and, when the model
supplies translations, up to two more, concurrently; Perplexity runs
beside them, unbilled. Until this change each Kagi query was asked for 10
hits alone or 6 when fanned out, the lists were merged by URL — the user's
query first, then each translation's new URLs, then Perplexity's — and
cut at 12. `search_secondhand` draws on the same 15-query allowance, one
query per site it searches on Kagi.

## What was measured

- **Translations are rare.** 149 of 175 calls (85%) asked one query; 14
  asked two and 12 asked three. Every call that succeeded returned exactly
  12 results: the cap always bit, even alone (10 from Kagi plus at least
  two new ones from Perplexity).
- **A run spends three queries, not fifteen.** Median 3 Kagi queries a
  run, mean 3.4, at most 12 — counting a `search_secondhand` call as one
  query where the tool bills one per site, so these are floors (20 such
  calls in the window). No `search_web` call was refused for want of the
  allowance, and no run was simulated to reach it; `search_secondhand`
  reported a site not searched for budget twice. 47 of 65 runs spent
  three or fewer; 9 spent more than five. Across the window, 221 queries
  in all — at Kagi's list price of $0.025 a query (an assumption, not
  checked against the invoice), $5.53 over the 33 days, about $5 a month.
- **Later searches still find new pages.** The second to fifth searches
  in a run return URLs no earlier search in that run had: 89–98% of them
  on average (medians 92–100%). Only from the sixth on does that fall, to
  79%. The model asks different things, not the same thing again.
- **Later searches are opened less.** Of 119 pages the model went on to
  fetch, 91 came from a search result; the other 28 came from elsewhere —
  a second-hand search, a remembered fact, the conversation, or what the
  model knew. Per call that succeeded, the first search in a run led to
  0.85 opened pages, the second 0.51, the third 0.30, the fourth 0.33,
  the fifth 0.20, and none from the sixth on.
- **Translated calls are opened more.** A single-query call led to 0.43
  opened pages (64 over 149); a two- or three-query call to about one (14
  over 14, 13 over 12). In fanned calls the opened pages split evenly
  between the first query's six slots and the six after them,
  translations and Perplexity together — 14 to 13.
- **The third query was paid for and thrown away.** With three queries
  Kagi returned 18 hits for 12 slots; the first two languages filled the
  cap unless they overlapped, so the third's results were cut before the
  model saw them. The merged list does not record which query a hit came
  from, so the traces cannot say how often that happened — only that the
  shape guaranteed it.

## What follows

- The allowance stays at 15: nobody reaches it, and lowering it to the
  level that would save money (six) would refuse a search in one run in
  seven to save about a tenth of the spend.
- Every Kagi query is now asked for its full ten hits — Kagi bills the
  query, not the hits — and the merge shares the 12 slots out: 10 for a
  query alone, 6 each for two, 4 each for three. A list that comes back
  short hands its spare slots to the other queries' deeper hits, the
  user's own first, then to Perplexity. The trade-off, stated: in a
  three-query call the user's own query has four slots where it had six.
  The traces showed its six slots and the six after them opened equally
  (14 to 13), so nothing measured says the two lost outrank the third
  language's — and a translation that finds little gives them back.
  Perplexity reaches a fanned list only through slots the Kagi lists
  leave, as before.
- `/metrics` carries the exact tally from here on —
  `scout_search_results_total{source, outcome}` counts, per Kagi query
  position and for Perplexity, the hits kept, the duplicates of an earlier
  entry, and the ones cut — depth the list had no room for, free hits now
  rather than a billed query's whole answer; `scout_search_calls_total
  {queries}` counts calls by the Kagi queries they spent, a refusal as
  zero. The question the next reading answers: is `kagi_3`'s `kept` ever
  above zero, and how `perplexity`'s compares.
- The fan-out is not where the money goes. If Kagi is still the dominant
  cost line, it is the price of the first three queries of every run, and
  the lever is Perplexity's share of the list, not the cap.
