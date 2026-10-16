#!/usr/bin/env python3
"""What the run traces say about the search fan-out: how many Kagi queries a
request spends, whether later searches in a run still find new pages, and
which results the model went on to open. Aggregates only — nothing here
prints a query, a URL or a snippet, so the output can be pasted anywhere.

Reads a copy of the database, never the live one (DuckDB is single-writer
and the pod holds it). The nightly backup is the copy:

    ssh $SCOUT_SSH 'kubectl -n scout exec deploy/scout -- cat /data/backups/<name>.duckdb' > /tmp/scout.duckdb
    scripts/search-fanout.py /tmp/scout.duckdb

Needs the `duckdb` Python package. Traces are kept for the last 300 runs
(`Core::TRACE_RUNS_KEEP`), so this is a rolling window, not history.

How to read it: search_web merges, in order, Kagi for the user's own query
(10 hits alone, 6 when translations ride along), then each translation's
new URLs, then Perplexity's, and cuts the list at 12. The merged list does
not say which query a hit came from, so the attribution by position below
is a lower bound; `/metrics` carries the exact per-query tally since
2026-10-16 (`scout_search_results_total`).
"""
import collections
import json
import statistics
import sys

import duckdb

BUDGET = 15          # inbox::budget::QUERIES_PER_REQUEST
MAX_QUERIES = 3      # per search_web call
MERGED_LIMIT = 12
PER_QUERY_ALONE = 10
PER_QUERY_FANNED = 6


def loads(text):
    try:
        return json.loads(text) if text else None
    except (TypeError, ValueError):
        return None


def asked_queries(args):
    """How many Kagi queries a call asked for, as the tool counts them."""
    if not isinstance(args, dict):
        return 0
    q = args.get("query", "")
    queries = [q.strip()] if isinstance(q, str) and q.strip() else []
    for extra in args.get("also_queries") or []:
        if isinstance(extra, str):
            e = extra.strip()
            if e and e not in queries and len(queries) < MAX_QUERIES:
                queries.append(e)
    return len(queries)


def pct(a, b):
    return f"{100 * a / b:.0f}%" if b else "n/a"


def main(path):
    con = duckdb.connect(path, read_only=True)
    rows = con.sql(
        """SELECT run_id, seq, tool, args, status, detail, result, nested, duration_ms
           FROM run_traces WHERE kind = 'tool' ORDER BY run_id, seq"""
    ).fetchall()
    outcome_of = dict(con.sql("SELECT id, outcome FROM runs").fetchall())

    by_run = collections.defaultdict(list)
    for r in rows:
        by_run[r[0]].append(r)

    calls = []
    runs_with_search = 0
    budget_hit_runs = 0
    budget_errors = 0
    kagi_per_run = []
    searches_per_run = []
    secondhand_calls = 0
    novelty_by_index = collections.defaultdict(list)
    fetched_pos = collections.Counter()
    fetched_call = collections.Counter()
    fetched_asked = collections.Counter()
    fetched_exact = collections.Counter()
    fetched_total = 0
    fetched_elsewhere = 0

    for trs in by_run.values():
        used = 0
        hit_budget = False
        seen = {}
        n_search = 0
        fetched = []
        for (_, _, tool, args, status, detail, result, nested, dur) in trs:
            a = loads(args)
            if tool == "search_web":
                asked = asked_queries(a)
                granted = min(asked, max(0, BUDGET - used))
                hit_budget |= granted < asked
                used += granted
                res = loads(result) if status == "ok" else None
                urls = [h.get("url") for h in res] if isinstance(res, list) else []
                urls = [u for u in urls if isinstance(u, str) and u]
                new = [u for u in urls if u not in seen]
                idx = n_search
                n_search += 1
                if urls:
                    novelty_by_index[min(idx, 5)].append(len(new) / len(urls))
                for pos, u in enumerate(urls):
                    seen.setdefault(u, (idx, pos, asked))
                calls.append(dict(asked=asked, n=len(urls), status=status, dur=dur, nested=nested))
                if detail and "budget spent" in detail:
                    budget_errors += 1
            elif tool == "search_secondhand":
                secondhand_calls += 1
                # One query per site asked; counted as one here, a floor.
                used += min(1, max(0, BUDGET - used))
            elif tool == "fetch_page":
                u = a.get("url") if isinstance(a, dict) else None
                if isinstance(u, str):
                    fetched.append(u.rstrip("/"))
        if n_search:
            runs_with_search += 1
            searches_per_run.append(n_search)
            kagi_per_run.append(used)
            budget_hit_runs += hit_budget
        seen_trim = {u.rstrip("/"): v for u, v in seen.items()}
        for u in fetched:
            fetched_total += 1
            if u not in seen_trim:
                fetched_elsewhere += 1
                continue
            idx, pos, asked = seen_trim[u]
            fetched_call[min(idx, 5)] += 1
            fetched_asked[asked] += 1
            if asked >= 2:
                fetched_exact[pos] += 1
                fetched_pos["fanned: first 6 (query 1)" if pos < PER_QUERY_FANNED else "fanned: 7-12 (translations/Perplexity)"] += 1
            else:
                fetched_pos["alone: first 10 (query 1)" if pos < PER_QUERY_ALONE else "alone: 11-12 (Perplexity)"] += 1

    print(f"runs: {len(outcome_of)} (with traces: {len(by_run)}; with a search: {runs_with_search})")
    print(f"search_web calls: {len(calls)} (nested: {sum(c['nested'] for c in calls)}); search_secondhand calls: {secondhand_calls}")
    print()
    print("== queries asked per search_web call (query + translations)")
    for k in sorted({c['asked'] for c in calls}):
        sub = [c for c in calls if c['asked'] == k]
        ok = [c for c in sub if c['status'] == 'ok']
        ns = [c['n'] for c in ok]
        durs = [c['dur'] for c in ok if c['dur']]
        print(f"  {k} queries: {len(sub)} calls ({pct(len(sub), len(calls))}); ok {len(ok)}; "
              f"median results {statistics.median(ns) if ns else 0:.0f}; "
              f"at the 12 cap {pct(sum(1 for n in ns if n >= MERGED_LIMIT), len(ok))}; "
              f"median duration {statistics.median(durs) if durs else 0:.0f} ms")
    print()
    print("== Kagi queries per run with a search (simulated against the budget)")
    if kagi_per_run:
        print(f"  median {statistics.median(kagi_per_run):.0f}, mean {statistics.mean(kagi_per_run):.1f}, max {max(kagi_per_run)}; "
              f"runs that hit the budget: {budget_hit_runs} ({pct(budget_hit_runs, runs_with_search)}); "
              f"'budget spent' refusals seen: {budget_errors}")
        hist = collections.Counter(min(k, BUDGET) for k in kagi_per_run)
        print("  histogram:", ", ".join(f"{k}:{v}" for k, v in sorted(hist.items())))
        print(f"  searches per run: median {statistics.median(searches_per_run):.0f}, max {max(searches_per_run)}")
    print()
    print("== novelty: share of a call's URLs no earlier call in the run returned, by call index")
    for idx in sorted(novelty_by_index):
        v = novelty_by_index[idx]
        print(f"  call #{idx + 1}{'+' if idx == 5 else ''}: n={len(v)}, mean new {statistics.mean(v):.0%}, median {statistics.median(v):.0%}")
    print()
    print("== what the model went on to open (fetch_page), by where the URL first appeared")
    from_search = fetched_total - fetched_elsewhere
    print(f"  fetched: {fetched_total}; from a search result: {from_search} ({pct(from_search, fetched_total)}); typed by the model or elsewhere: {fetched_elsewhere}")
    for k, v in sorted(fetched_pos.items()):
        print(f"  {k}: {v}")
    print("  by the call that first returned it:", ", ".join(f"#{k + 1}:{v}" for k, v in sorted(fetched_call.items())))
    print("  by queries asked in that call:", ", ".join(f"{k}q:{v}" for k, v in sorted(fetched_asked.items())))
    print("  exact position within fanned (>=2-query) calls:", ", ".join(f"{k + 1}:{v}" for k, v in sorted(fetched_exact.items())))
    print()
    oc = collections.Counter(outcome_of.get(r) for r, trs in by_run.items() if any(t[2] == 'search_web' for t in trs))
    print("== run outcomes among runs with a search:", dict(oc))


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
