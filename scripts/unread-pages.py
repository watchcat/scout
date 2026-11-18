#!/usr/bin/env python3
"""What the run traces say about pages Scout could not price: how fetch_page
reads ended, by host; which of the pages a run went on to offer came back
with no price; and how many offers reached compare_prices with no shipping
cost. Gate 1 of docs/superpowers/specs/2026-11-17-price-watch-and-shop-browsing-design.md.

It prints counts and host names. Never a path, a query, a title or an
amount. A host name is still something somebody looked at, and most hosts
are looked at by one person: so a host is named only when at least two
accounts used it (`--min-accounts`), and the rest are summed. With
`--min-accounts 1` every host is named — for the operator's own eyes, not
for a file anyone else can read.

Reads a copy of the database, never the live one (DuckDB is single-writer
and the pod holds it). The nightly backup is the copy:

    ssh $SCOUT_SSH 'kubectl -n scout exec deploy/scout -- cat /data/backups/<name>.duckdb' > /tmp/scout.duckdb
    scripts/unread-pages.py /tmp/scout.duckdb [--days 60] [--top 20] [--min-accounts 2]

Needs the `duckdb` Python package. Traces are kept for the last 300 runs
(`Core::TRACE_RUNS_KEEP`), so the window is whichever is shorter: the days
asked for — whole days, the newest trace's day being the last — or those
300 runs. Tests: python3 scripts/unread-pages.test.py

How to read it.

A fetch_page call ends one of six ways. *Priced*: the page's own data —
schema.org markup, microdata or the shop's embedded state — gave Rust a
price for the product at that address. *No price*: the page was read and
gave none; the model had its text and nothing else. *Blocked*: a wall
(401, 403, 429, 503, or a bot check served with a 200) that the headless
render did not clear either. *Gone*: 404 or 410. *Failed*: anything else.
*Not opened*: refused rather than read — the run's five pages were spent,
or the address was not one to open. A call the run died in the middle of
has no outcome and is left out.

The traces do not say which reads needed the headless render. fetch_page
tries it when the plain request is refused or comes back an empty shell,
and hands back the same shape either way; a call's duration does not tell
them apart, since it is measured across the whole batch of calls it ran
in. That split has to be measured, not read off: the Shop reach card reads
each host both ways.

Most pages fetch_page opens are not product pages — reviews, forums,
listings — and a page with no price is no failure there. Two things mark a
page as a product page, and neither is a guess about its address: the run
went on to hand that address to compare_prices as an offer, or the page's
own markup named a product. Both counts are printed. The second mark
leans towards priced pages — markup that names a product nearly always
carries its price — so the two together give the lower share, and the rule
is read off that one.

Where an offered page was read with no price, the last question is whether
the page's text wrote the amount the model offered — by the extractor's
own rule, `text_writes_amount` in tools/fetch.rs. Where it did ("in text"
in the table), the model found its number on the page; that does not make
it the product's price, which is why the extractor does not read prices
out of text. Where it did not, the amount came from somewhere else, or
from past the 6,000 characters of text the tool keeps.

Shipping is set beside whether the offered page was read — read off the
offer the model handed over, not off the page: none stated, free, or a
cost. Listings that search_secondhand finds through an API are counted
apart from shops: eBay's arrive with their shipping cost, Marktplaats's
with none, and neither says anything about what reading a page gives.
"""
import argparse
import collections
import datetime
import json
import re
from urllib.parse import urlsplit

PRICED = "priced"
NO_PRICE = "no price"
BLOCKED = "blocked"
GONE = "gone"
FAILED = "failed"
NOT_OPENED = "not opened"
READ = (PRICED, NO_PRICE)
# Which outcome stands for a page a run asked for more than once.
BEST = {PRICED: 5, NO_PRICE: 4, BLOCKED: 3, GONE: 2, FAILED: 1, NOT_OPENED: 0}

# The rule the spec named before anything was measured, as (at least, in).
NO_PRICE_AT_LEAST = (1, 10)     # of product pages read
NO_SHIPPING_AT_LEAST = (1, 3)   # of offers handed to compare_prices

WALL = re.compile(r"HTTP (401|403|429|503)\b|served a bot check")
MISSING = re.compile(r"HTTP (404|410)\b")
BUDGET = "page budget spent"
REFUSED = (BUDGET, "invalid url", "unsupported url scheme", "not on the public internet")
# A DNS name and nothing else: an address field can hold whatever the model
# put there, and what is printed has to be a host.
NAME = re.compile(r"\A(?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\.)+[a-z][a-z0-9-]*\Z")
NO_HOST = "(no host)"


def loads(text):
    try:
        return json.loads(text) if text else None
    except (TypeError, ValueError):
        return None


def host(url):
    """The host and nothing after it, lower-cased, without a leading www."""
    try:
        name = (urlsplit(url).hostname or "").lower()
    except (TypeError, ValueError, AttributeError):
        return NO_HOST
    if name.startswith("www."):
        name = name[4:]
    return name if NAME.match(name) else NO_HOST


def listed_by_api(name):
    """Hosts whose listings search_secondhand brings with their facts."""
    return name == "link.marktplaats.nl" or name.startswith("ebay.")


def page_key(url):
    """A page as fetch_page's own memory keys it: the address less a trailing slash."""
    return url.rstrip("/") if isinstance(url, str) else ""


def outcome(status, detail, result):
    """How one fetch_page call ended."""
    if status == "ok":
        page = loads(result)
        if not isinstance(page, dict):
            return FAILED  # not the tool's own shape, a cut-off result included: counted, never guessed at
        product = page.get("product")
        return PRICED if isinstance(product, dict) and product.get("price") else NO_PRICE
    text = detail or ""
    if any(words in text for words in REFUSED):
        return NOT_OPENED
    if WALL.search(text):
        return BLOCKED
    if MISSING.search(text):
        return GONE
    return FAILED


def text_writes_amount(text, amount):
    """Whether the text writes this amount as a price: "12.99" or "12,99",
    and not as a piece of a longer number. The extractor's rule, ASCII
    digits as there."""
    point = f"{float(amount):.2f}"
    return any(
        re.search(r"(?<![0-9.,])" + re.escape(needle) + r"(?![0-9.,])", text)
        for needle in (point, point.replace(".", ","))
    )


def shortfalls(counts):
    """What fell short on a host: offers with no shipping cost, offered pages
    read with no price, reads a wall refused."""
    return counts["no_shipping"] + counts["offered_no_price"] + counts[BLOCKED]


def requested(counts):
    return sum(counts[kind] for kind in (PRICED, NO_PRICE, BLOCKED, GONE, FAILED))


def ranked(hosts):
    """Hosts by shortfalls, then by pages asked for there, then by name."""
    return sorted(hosts.items(), key=lambda item: (-shortfalls(item[1]), -requested(item[1]), item[0]))


def measure(rows, accounts=None):
    """Counts from tool rows `(run_id, seq, tool, args, status, detail, result)`.
    `accounts` maps a run to whose it was; a run it does not know counts for
    nobody, so that a host is never named on the strength of it."""
    accounts = accounts or {}
    by_run = collections.defaultdict(list)
    for row in rows:
        by_run[row[0]].append(row)

    fetch = collections.Counter()
    offers = collections.Counter()
    offered = collections.Counter()
    product = collections.Counter()
    hosts = collections.defaultdict(collections.Counter)
    seen_in = collections.defaultdict(set)   # host -> runs
    used_by = collections.defaultdict(set)   # host -> accounts

    for run_id, run_rows in by_run.items():
        run_rows.sort(key=lambda row: row[1])
        pages = {}    # key -> (outcome, page as read or None)
        asked = {}    # key -> the first offer that named it
        fetched = refused = False

        def touch(name):
            seen_in[name].add(run_id)
            if accounts.get(run_id) is not None:
                used_by[name].add(accounts[run_id])
            return hosts[name]

        for (_, _, tool, args, status, detail, result) in run_rows:
            a = loads(args)
            a = a if isinstance(a, dict) else {}
            if tool == "fetch_page":
                fetch["calls"] += 1
                fetched = True
                url = a.get("url")
                key = page_key(url)
                earlier = pages.get(key)
                if earlier and earlier[0] in READ:
                    fetch["again"] += 1  # served from the run's own memory; no request was made
                    continue
                kind = outcome(status, detail, result)
                refused |= kind == NOT_OPENED and BUDGET in (detail or "")
                fetch[kind] += 1
                touch(host(url))[kind] += 1
                if earlier is None or BEST[kind] > BEST[earlier[0]]:
                    pages[key] = (kind, loads(result) if kind in READ else None)
            elif tool == "compare_prices" and status == "ok":
                given = [o for o in (a.get("offers") or []) if isinstance(o, dict)]
                offers["calls"] += 1
                offers["offers"] += len(given)
                unstated = [o for o in given if o.get("shipping") is None]
                offers["no_shipping"] += len(unstated)
                offers["calls_stating_none"] += bool(given) and len(unstated) == len(given)
                for o in given:
                    counts = touch(host(o.get("url")))
                    counts["offers"] += 1
                    counts["no_shipping"] += o.get("shipping") is None
                    asked.setdefault(page_key(o.get("url")), o)

        fetch["runs"] += fetched
        fetch["runs_refused"] += refused
        for key, o in asked.items():
            name = host(o.get("url"))
            unstated = o.get("shipping") is None
            free = (not unstated) and o.get("shipping") == 0
            if key not in pages:
                offered["never_opened"] += 1
                offered["never_opened_no_shipping"] += unstated
                offered["never_opened_free"] += free
                if listed_by_api(name):
                    offered["never_opened_listed"] += 1
                    offered["never_opened_listed_no_shipping"] += unstated
                    offered["never_opened_listed_free"] += free
                continue
            kind, page = pages[key]
            offered[kind] += 1
            if kind not in READ:
                offered["unread"] += 1
                offered["unread_no_shipping"] += unstated
                continue
            offered["read_no_shipping"] += unstated
            offered["read_shipping_stated"] += not unstated
            offered["read_shipping_free"] += free
            counts = hosts[name]
            product["read"] += 1
            product["offered_read"] += 1
            if kind == PRICED:
                counts["offered_priced"] += 1
                continue
            counts["offered_no_price"] += 1
            product["no_price"] += 1
            product["offered_no_price"] += 1
            try:
                wrote = text_writes_amount(str(page.get("text") or ""), o.get("price"))
            except (TypeError, ValueError):
                wrote = False
            offered["text_writes"] += wrote
            counts["offered_in_text"] += wrote
            offered["text_cut"] += (not wrote) and bool(page.get("truncated"))
        for key, (kind, page) in pages.items():
            if key in asked or kind not in READ or not isinstance(page.get("product"), dict):
                continue
            product["read"] += 1
            product["markup_only"] += 1
            if kind == NO_PRICE:
                product["no_price"] += 1
                product["markup_only_no_price"] += 1

    for name, counts in hosts.items():
        counts["runs"] = len(seen_in[name])
        counts["accounts"] = len(used_by[name])
    fetch["requested"] = requested(fetch)
    return {"fetch": fetch, "offers": offers, "offered": offered, "product": product, "hosts": dict(hosts)}


def at_least(part, whole, threshold):
    """Whether `part` of `whole` reaches a threshold given as (n, in): whole numbers, no rounding."""
    n, of = threshold
    return whole > 0 and part * of >= whole * n


def rule(m):
    """The "needed" half of the spec's rule, read off the two shares."""
    product, offers = m["product"], m["offers"]
    price_met = at_least(product["no_price"], product["read"], NO_PRICE_AT_LEAST)
    shipping_met = at_least(offers["no_shipping"], offers["offers"], NO_SHIPPING_AT_LEAST)
    return {
        "no_price_share": product["no_price"] / product["read"] if product["read"] else None,
        "no_shipping_share": offers["no_shipping"] / offers["offers"] if offers["offers"] else None,
        "price_met": price_met,
        "shipping_met": shipping_met,
        "needed": price_met or shipping_met,
    }


def pct(part, whole):
    return f"{100 * part / whole:.0f}%" if whole else "n/a"


def host_line(name, c, summed=False):
    behind = f"{'':>4} {'':>5}" if summed else f"{c['runs']:>4} {c['accounts']:>5}"
    return (
        f"  {name:<38.38} {behind} {c[PRICED]:>6} {c[NO_PRICE]:>8} {c[BLOCKED]:>7} {c[GONE] + c[FAILED]:>5} {c[NOT_OPENED]:>6}"
        f" | {c['offers']:>6} {c['no_shipping']:>7} | {c['offered_priced']:>6} {c['offered_no_price']:>8} {c['offered_in_text']:>7}"
        f" | {shortfalls(c):>5}"
    )


def summed(rows):
    total = collections.Counter()
    for _, counts in rows:
        total.update(counts)
    return total


def report(m, top, window, min_accounts=2):
    f, o, of, p = m["fetch"], m["offers"], m["offered"], m["product"]
    r = rule(m)
    said = lambda met: "met" if met else "not met"
    shops_unopened = of["never_opened"] - of["never_opened_listed"]
    shops_unopened_none = of["never_opened_no_shipping"] - of["never_opened_listed_no_shipping"]
    shops_unopened_free = of["never_opened_free"] - of["never_opened_listed_free"]
    out = [f"window: {window}", ""]
    out += [
        "== fetch_page",
        f"  calls: {f['calls']}, in {f['runs']} runs; the same page again in a run: {f['again']}",
        f"  refused rather than read: {f[NOT_OPENED]} — the page budget refused at least one in {f['runs_refused']} runs ({pct(f['runs_refused'], f['runs'])})",
        f"  pages asked for: {f['requested']}",
    ]
    for kind, label in ((PRICED, "read with a price"), (NO_PRICE, "read with no price"), (BLOCKED, "blocked"), (GONE, "gone"), (FAILED, "failed otherwise")):
        out.append(f"    {label:<20} {f[kind]:>5}  {pct(f[kind], f['requested'])}")
    out += [
        "",
        "== compare_prices",
        f"  calls: {o['calls']}; offers: {o['offers']}; with no shipping cost: {o['no_shipping']} ({pct(o['no_shipping'], o['offers'])})",
        f"  calls where no offer stated shipping: {o['calls_stating_none']} ({pct(o['calls_stating_none'], o['calls'])})",
        "",
        "== the pages behind the offers (one address counted once a run)",
        f"  never asked for: {of['never_opened']}; refused rather than read: {of[NOT_OPENED]}; "
        f"blocked: {of[BLOCKED]}; gone or failed: {of[GONE] + of[FAILED]}",
        f"  read: {p['offered_read']} — with a price {of[PRICED]}, with none {of[NO_PRICE]} ({pct(of[NO_PRICE], p['offered_read'])})",
        f"    of those with none, the page's text wrote the offered amount: {of['text_writes']}; "
        f"did not: {of[NO_PRICE] - of['text_writes']} (text cut at the tool's limit in {of['text_cut']} of them)",
        "",
        "== shipping, by whether the offered page was read",
        f"  page read: {of['read_no_shipping']} of {p['offered_read']} stated none ({pct(of['read_no_shipping'], p['offered_read'])}); "
        f"of the {of['read_shipping_stated']} that stated it, {of['read_shipping_free']} said free",
        f"  never asked for, a shop: {shops_unopened_none} of {shops_unopened} stated none ({pct(shops_unopened_none, shops_unopened)}); "
        f"of the {shops_unopened - shops_unopened_none} that stated it, {shops_unopened_free} said free",
        f"  never asked for, a listing found through an API: {of['never_opened_listed_no_shipping']} of {of['never_opened_listed']} stated none",
        f"  asked for and not read: {of['unread_no_shipping']} of {of['unread']} stated none",
        "",
        "== product pages (offered, or the markup named a product)",
        f"  offered and read: {p['offered_read']}, no price {p['offered_no_price']} ({pct(p['offered_no_price'], p['offered_read'])})",
        f"  markup only, not offered: {p['markup_only']}, no price {p['markup_only_no_price']}",
        f"  together: {p['read']}, no price {p['no_price']} ({pct(p['no_price'], p['read'])})",
        "",
        "== the rule (needed, if either holds)",
        f"  product pages read with no price: {pct(p['no_price'], p['read'])} — the rule asks one in ten: {said(r['price_met'])}",
        f"  offers with no shipping cost: {pct(o['no_shipping'], o['offers'])} — the rule asks one in three: {said(r['shipping_met'])}",
        f"  needed: {'yes' if r['needed'] else 'no'}",
        "",
        "== hosts, by what fell short there (offers with no shipping + offered pages with no price + blocked)",
        f"  {'':<38} {'runs':>4} {'accts':>5} {'priced':>6} {'no price':>8} {'blocked':>7} {'other':>5} {'unopen':>6}"
        f" | {'offers':>6} {'no ship':>7} | {'priced':>6} {'no price':>8} {'in text':>7} | {'short':>5}",
        f"  {'host':<38} {'':>4} {'':>5} {'— pages asked for —':^36} | {'— offers —':^14} | {'— offered pages read —':^23} |",
    ]
    order = ranked(m["hosts"])
    everyone = min_accounts <= 1
    named = [(name, c) for name, c in order if everyone or c["accounts"] >= min_accounts]
    unnamed = [(name, c) for name, c in order if not (everyone or c["accounts"] >= min_accounts)]
    for name, counts in named[:top]:
        out.append(host_line(name, counts))
    if named[top:]:
        out.append(host_line(f"{len(named[top:])} other hosts", summed(named[top:]), summed=True))
    if unnamed:
        out.append(host_line(f"{len(unnamed)} hosts fewer than {min_accounts} accounts used", summed(unnamed), summed=True))
    return "\n".join(out)


def read(path, days):
    """The window's fetch_page and compare_prices rows, and what the window was."""
    import duckdb

    con = duckdb.connect(path, read_only=True)
    oldest, newest = con.execute("SELECT min(started_at), max(started_at) FROM run_traces").fetchone()
    if newest is None:
        return [], {"since": None, "until": None, "runs": 0, "accounts": {}}
    first_day = newest.date() - datetime.timedelta(days=days - 1)
    rows = con.execute(
        """SELECT run_id, seq, tool, args, status, detail, result
           FROM run_traces
           WHERE kind = 'tool' AND tool IN ('fetch_page', 'compare_prices')
             AND status IS NOT NULL AND started_at >= ?
           ORDER BY run_id, seq""",
        [datetime.datetime.combine(first_day, datetime.time.min)],
    ).fetchall()
    runs = {row[0] for row in rows}
    try:
        owners = con.execute("SELECT id, account_id FROM runs").fetchall()
    except duckdb.Error:
        owners = []  # a copy without the table names no host
    window = {
        "since": str(max(first_day, oldest.date())),
        "until": str(newest.date()),
        "runs": len(runs),
        "accounts": {run: account for run, account in owners if run in runs and account is not None},
    }
    return rows, window


def main():
    parser = argparse.ArgumentParser(description="What the traces say about pages Scout could not price.")
    parser.add_argument("database", help="a copy of the database, such as a nightly backup")
    parser.add_argument("--days", type=int, default=60, help="whole days, ending on the newest trace's day (default 60)")
    parser.add_argument("--top", type=int, default=20, help="how many hosts to list before summing the rest (default 20)")
    parser.add_argument("--min-accounts", type=int, default=2,
                        help="name a host only when this many accounts used it (default 2; 1 names every host)")
    args = parser.parse_args()
    if args.days < 1:
        parser.error("--days counts whole days and needs at least one")
    rows, window = read(args.database, args.days)
    line = (
        f"{window['since']} to {window['until']} (asked for {args.days} days); "
        f"{window['runs']} runs of {len(set(window['accounts'].values()))} accounts opened a page or compared prices"
    )
    print(report(measure(rows, window["accounts"]), args.top, line, args.min_accounts))


if __name__ == "__main__":
    main()
