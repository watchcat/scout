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
this is a rolling window, not history. Tests: scripts/flight-cost.test.py.

How it counts. A flight-desk run is a run whose trace has an `ask_flights`
call. Its day-searches are the `flight_search` rows `request_log` holds for
the same account between the run's start and its end: `search_flights`
writes one for every day it buys, so a repeat served from the request's
memo is not one. A run with no end recorded is skipped, and said. Re-pricing
searches and Ignav booking lookups are not in `request_log`. They are
counted from `finalise_trip`'s result (each flight re-priced at both
providers, plus Duffel's multi-city request when there are two or more) and
from the booking-link calls that answered.

How it prices. List prices as read on 2026-11-26 (`docs/how-flight-search-works.md`,
sections 7.1 and 7.9): MiniMax M3 per token, uncached input, cached input
and output apart; a Duffel search at the excess-search fee, since Scout has
no orders; an Ignav request past its one-time free 1,000. A run's tokens
are its own and its flight desk's; the small side calls are not in them.
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
        value = json.loads(text) if text else None
    except ValueError:
        return None
    return value if isinstance(value, dict) else None


def model_cost(tin, cached, tout):
    return (tin - cached) * PRICE_IN + cached * PRICE_CACHED + tout * PRICE_OUT


def provider_cost(bought, repriced, multi, lookups):
    """Duffel and Ignav, in dollars. A bought day and a re-priced flight are
    a request at each provider; the multi-city request is Duffel's alone and
    a booking lookup Ignav's alone."""
    return (bought + repriced + multi) * DUFFEL_SEARCH + (bought + repriced + lookups) * IGNAV_REQUEST


def measure(c):
    """One dict per flight-desk run that carries tokens, oldest first, and
    the number of such runs skipped for want of an end."""
    flight_runs = [r[0] for r in c.execute("select distinct run_id from run_traces where tool = 'ask_flights'").fetchall()]
    rows, skipped = [], 0
    for start, end, acct, tin, cached, tout, rid in c.execute(
        "select started_at, ended_at, account_id, input_tokens, cached_tokens, output_tokens, id from runs "
        "where input_tokens is not null and id in (select unnest(?)) order by started_at", [flight_runs]).fetchall():
        if end is None:
            skipped += 1
            continue
        calls = c.execute(
            "select tool, args, status, result, truncated from run_traces where run_id = ? and tool is not null",
            [rid]).fetchall()
        searches = [(loads(a) or {}, s) for t, a, s, _, _ in calls if t == "search_flights" and a is not None]
        answered = [args for args, status in searches if status == "ok"]
        bought = c.execute(
            "select count(*) from request_log where kind = 'flight_search' and account_id = ? "
            "and created_at between ? and ?", [acct, start, end]).fetchone()[0]
        repriced = multi = unread = 0
        lookups = sum(1 for t, a, s, _, _ in calls if t == "flight_booking_links" and a is not None and s == "ok")
        for t, a, s, result, truncated in calls:
            if t != "finalise_trip" or a is None or s != "ok":
                continue
            out = None if truncated else loads(result)
            if out is None:
                unread += 1
                continue
            segments = out.get("segments") or []
            repriced += len(segments)
            multi += 1 if len(segments) >= 2 else 0
            lookups += sum(1 for seg in segments if isinstance(seg, dict) and seg.get("booking"))
        rows.append(dict(
            week=f"{start:%G-W%V}",
            calls=len(searches),
            flex=max([int(args.get("flex_days") or 0) for args in answered] or [0]),
            ret=any(args.get("return_date") for args in answered),
            bought=bought, repriced=repriced, multi=multi, lookups=lookups, unread_finalise=unread,
            tin=tin, cached=cached or 0, tout=tout,
            model=model_cost(tin, cached or 0, tout),
            providers=provider_cost(bought, repriced, multi, lookups),
        ))
    return rows, skipped


def main(path):
    c = duckdb.connect(path, read_only=True)
    first, last, runs, with_tokens = c.execute(
        "select min(started_at), max(started_at), count(*), count(input_tokens) from runs").fetchone()
    if not runs:
        print("no runs in this database")
        return
    print(f"runs: {runs}, {first:%Y-%m-%d} to {last:%Y-%m-%d}; {with_tokens} carry tokens")
    logged, since, until = c.execute(
        "select count(*), min(created_at), max(created_at) from request_log where kind = 'flight_search'").fetchone()
    if logged:
        print(f"day-searches logged: {logged}, {since:%Y-%m-%d} to {until:%Y-%m-%d} "
              f"(at least ${logged * DUFFEL_SEARCH:.2f} at Duffel's excess-search fee)")

    rows, skipped = measure(c)
    print(f"flight-desk runs with tokens: {len(rows)}" + (f"; {skipped} skipped, no end recorded" if skipped else ""))
    print("\nweek | search calls | window | return | day-searches | re-priced | multi-city | lookups | in | cached | out | model $ | providers $ | total $")
    groups = collections.defaultdict(list)
    for r in rows:
        print(f"{r['week']} | {r['calls']} | ±{r['flex']} | {'yes' if r['ret'] else 'no'} | {r['bought']} | {r['repriced']} | "
              f"{r['multi']} | {r['lookups']} | {r['tin']:,} | {r['cached']:,} ({r['cached'] / r['tin']:.0%}) | {r['tout']:,} | "
              f"{r['model']:.3f} | {r['providers']:.3f} | {r['model'] + r['providers']:.3f}"
              + (f" | {r['unread_finalise']} finalise result(s) unreadable" if r["unread_finalise"] else ""))
        kind = "no search" if not r["calls"] else ("window" if r["flex"] else "one date")
        groups[kind].append(r["model"] + r["providers"])
    for kind, totals in groups.items():
        print(f"  {kind}: {len(totals)} runs, total $ median {statistics.median(totals):.3f}")

    print("\nevery run with tokens, by week: runs, input, cached share, output, model $")
    all_in = all_cached = all_out = counted = 0
    for week, n, tin, cached, tout in c.execute(
        "select strftime(started_at, '%G-W%V'), count(*), sum(input_tokens), sum(cached_tokens), sum(output_tokens) "
        "from runs where input_tokens is not null group by 1 order by 1").fetchall():
        print(f"  {week}: {n}, {tin:,}, {cached / tin:.0%}, {tout:,}, {model_cost(tin, cached, tout):.2f}")
        all_in, all_cached, all_out, counted = all_in + tin, all_cached + cached, all_out + tout, counted + n
    if counted:
        total = model_cost(all_in, all_cached, all_out)
        print(f"  all: {counted} runs, cached {all_cached / all_in:.0%}, model ${total:.2f}, ${total / counted:.3f} a run")


if __name__ == "__main__":
    main(sys.argv[1])
