#!/usr/bin/env python3
"""What a flight question costs Scout, read from the run traces and token
counts. Aggregates only — nothing here prints a brief, a route, a date of
travel, a name or a message, so the output can be pasted anywhere.

Reads a copy of the database, never the live one (DuckDB is single-writer
and the pod holds it). The nightly backup is the copy:

    ssh $SCOUT_SSH 'kubectl -n scout exec deploy/scout -- cat /data/backups/<name>.duckdb' > /tmp/scout.duckdb
    scripts/flight-cost.py /tmp/scout.duckdb

Needs the `duckdb` Python package. Runs carry token counts from 2026-10-17
on, and traces are kept for the last 300 runs (`Core::TRACE_RUNS_KEEP`), so
this is a rolling window, not history.

How it counts. A flight-desk run is a run whose trace has an `ask_flights`
call. Its day-searches are the `flight_search` rows `request_log` holds for
the same account inside the run's span: `search_flights` writes one for
every day it buys, so a repeat served from the request's memo is not one.
Re-pricing searches and Ignav booking lookups are not in `request_log`; they
are counted from `finalise_trip`'s result and the booking tools' calls.

How it prices. List prices as read on 2026-11-26 (`docs/how-flight-search-works.md`,
section 7.9): MiniMax M3 per token, uncached input, cached input and output
apart; a Duffel search at the excess-search fee, since Scout has no orders;
an Ignav request past its one-time free 1,000.
"""
import collections
import json
import statistics
import sys

import duckdb

PRICE_IN = 0.30 / 1e6
PRICE_CACHED = 0.06 / 1e6
PRICE_OUT = 1.20 / 1e6
DUFFEL_SEARCH = 0.005
IGNAV_REQUEST = 0.002


def loads(text):
    try:
        return json.loads(text) if text else None
    except ValueError:
        return None


def model_cost(tin, cached, tout):
    return (tin - cached) * PRICE_IN + cached * PRICE_CACHED + tout * PRICE_OUT


def main(path):
    c = duckdb.connect(path, read_only=True)
    span = c.execute("select min(started_at), max(started_at), count(*), count(input_tokens) from runs").fetchone()
    print(f"runs: {span[2]}, {span[0]:%Y-%m-%d} to {span[1]:%Y-%m-%d}; {span[3]} carry tokens")
    logged = c.execute(
        "select count(*), min(created_at), max(created_at) from request_log where kind = 'flight_search'").fetchone()
    if logged[0]:
        print(f"day-searches logged: {logged[0]}, {logged[1]:%Y-%m-%d} to {logged[2]:%Y-%m-%d}")

    flight_runs = [r[0] for r in c.execute("select distinct run_id from run_traces where tool = 'ask_flights'").fetchall()]
    print(f"traced flight-desk runs: {len(flight_runs)}\n")
    print("week | searches | window | return | day-searches | re-price | Ignav lookups | in | cached | out | model $ | providers $ | total $")
    groups = collections.defaultdict(list)
    for start, end, acct, tin, cached, tout, rid in c.execute(
        "select started_at, ended_at, account_id, input_tokens, cached_tokens, output_tokens, id from runs "
        "where input_tokens is not null and id in (select unnest(?)) order by started_at", [flight_runs]).fetchall():
        calls = c.execute("select tool, args, result from run_traces where run_id = ? and tool is not null", [rid]).fetchall()
        searches = [loads(a) or {} for t, a, _ in calls if t == "search_flights" and a is not None]
        flex = max([int(s.get("flex_days") or 0) for s in searches] or [0])
        ret = any(s.get("return_date") for s in searches)
        bought = c.execute(
            "select count(*) from request_log where kind = 'flight_search' and account_id = ? "
            "and created_at between ? - interval 5 second and ? + interval 5 second",
            [acct, start, end or start]).fetchone()[0]
        reprice, lookups = 0, sum(1 for t, a, _ in calls if t == "flight_booking_links" and a is not None)
        for t, _, result in calls:
            if t == "finalise_trip" and result:
                segments = (loads(result) or {}).get("segments") or []
                reprice += len(segments) + (1 if len(segments) >= 2 else 0)
                lookups += sum(1 for s in segments if s.get("booking"))
        model = model_cost(tin, cached or 0, tout)
        providers = (bought + reprice) * DUFFEL_SEARCH + (bought + reprice - (1 if reprice else 0) + lookups) * IGNAV_REQUEST
        print(f"{start:%G-W%V} | {len(searches)} | ±{flex} | {'yes' if ret else 'no'} | {bought} | {reprice} | {lookups} | "
              f"{tin:,} | {cached:,} ({cached / tin:.0%}) | {tout:,} | {model:.3f} | {providers:.3f} | {model + providers:.3f}")
        kind = "no search" if not searches else ("window" if flex else "one date")
        groups[kind].append(model + providers)
    for kind, totals in groups.items():
        print(f"  {kind}: {len(totals)} runs, total $ median {statistics.median(totals):.3f}")

    print("\nevery run with tokens, by week: runs, input, cached share, output, model $")
    all_in = all_cached = all_out = runs = 0
    for week, n, tin, cached, tout in c.execute(
        "select strftime(started_at, '%G-W%V'), count(*), sum(input_tokens), sum(cached_tokens), sum(output_tokens) "
        "from runs where input_tokens is not null group by 1 order by 1").fetchall():
        print(f"  {week}: {n}, {tin:,}, {cached / tin:.0%}, {tout:,}, {model_cost(tin, cached, tout):.2f}")
        all_in, all_cached, all_out, runs = all_in + tin, all_cached + cached, all_out + tout, runs + n
    if runs:
        total = model_cost(all_in, all_cached, all_out)
        print(f"  all: {runs} runs, cached {all_cached / all_in:.0%}, model ${total:.2f}, ${total / runs:.3f} a run")


if __name__ == "__main__":
    main(sys.argv[1])
