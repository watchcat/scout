#!/usr/bin/env python3
"""Tests for unread-pages.py: python3 scripts/unread-pages.test.py

Every trace here is written out, so each number the script prints is one a
reader can count by hand. Needs the `duckdb` package, as the script does.
"""
import datetime
import importlib.util
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest

import duckdb

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("unread_pages", HERE / "unread-pages.py")
up = importlib.util.module_from_spec(spec)
spec.loader.exec_module(up)


def page(price=None, product=True, text="", truncated=False):
    """What fetch_page hands back, as the trace keeps it."""
    prod = {"name": "A thing", "price": price, "availability": None, "seller": None} if product else None
    return json.dumps({"text": text, "truncated": truncated, "links": [], "availability": None, "product": prod})


def fetch(run, seq, url, result=None, detail=None):
    """A fetch_page row: a result means it answered, a detail that it failed."""
    status = "ok" if result is not None else "failed"
    return (run, seq, "fetch_page", json.dumps({"url": url}), status, detail, result if result is not None else detail)


def compare(run, seq, offers, status="ok"):
    return (run, seq, "compare_prices", json.dumps({"unit_name": "piece", "offers": offers}), status, None, "{}")


def offer(url, price, shipping=None):
    o = {"title": "A thing", "url": url, "price": price, "currency": "EUR"}
    if shipping is not None:
        o["shipping"] = shipping
    return o


class Outcome(unittest.TestCase):
    def test_a_read_is_priced_only_when_the_page_s_own_data_gave_a_price(self):
        self.assertEqual(up.outcome("ok", None, page(price="13.80 EUR")), up.PRICED)
        self.assertEqual(up.outcome("ok", None, page(price=None)), up.NO_PRICE)
        self.assertEqual(up.outcome("ok", None, page(product=False)), up.NO_PRICE)

    def test_a_wall_is_blocked_whether_it_answers_with_a_status_or_a_challenge(self):
        for detail in (
            "page returned HTTP 403 Forbidden",
            "page returned HTTP 401 Unauthorized",
            "page returned HTTP 429 Too Many Requests",
            "page returned HTTP 503 Service Unavailable",
            "shop.example served a bot check instead of the page, and it could not be cleared",
        ):
            self.assertEqual(up.outcome("failed", detail, detail), up.BLOCKED, detail)

    def test_a_page_that_is_gone_is_not_a_page_that_refused(self):
        self.assertEqual(up.outcome("failed", "page returned HTTP 404 Not Found", None), up.GONE)
        self.assertEqual(up.outcome("failed", "page returned HTTP 410 Gone", None), up.GONE)

    def test_a_refusal_before_any_request_is_not_a_read_at_all(self):
        self.assertEqual(
            up.outcome("failed", "page budget spent (5 opens per request) — answer with what you already have", None),
            up.NOT_OPENED,
        )
        self.assertEqual(up.outcome("failed", "invalid url: relative URL without a base", None), up.NOT_OPENED)
        self.assertEqual(up.outcome("failed", "unsupported url scheme: ftp", None), up.NOT_OPENED)
        self.assertEqual(
            up.outcome("failed", "that address is not on the public internet — it cannot be opened", None),
            up.NOT_OPENED,
        )

    def test_anything_else_that_failed_is_only_failed(self):
        self.assertEqual(up.outcome("failed", "fetch failed: error following redirect for url (https://x.example/)", None), up.FAILED)
        self.assertEqual(up.outcome("failed", "page returned HTTP 400 Bad Request", None), up.FAILED)
        # An answer that is not the tool's own shape: counted, never guessed at.
        self.assertEqual(up.outcome("ok", None, "[1, 2"), up.FAILED)


class Host(unittest.TestCase):
    def test_a_host_is_lower_case_without_www_and_never_carries_the_path(self):
        self.assertEqual(up.host("https://WWW.Shop.Example/p/123?x=1"), "shop.example")
        self.assertEqual(up.host("https://m.shop.example/p"), "m.shop.example")
        self.assertEqual(up.host("not a url"), "(no host)")
        self.assertEqual(up.host(None), "(no host)")

    def test_what_is_not_shaped_like_a_name_is_never_printed_as_one(self):
        # A model can put words where an address belongs; the trace keeps them.
        self.assertEqual(up.host("https://some words a person typed"), "(no host)")
        self.assertEqual(up.host("https://192.168.1.20/admin"), "(no host)")
        self.assertEqual(up.host("https://[::1]/"), "(no host)")
        self.assertEqual(up.host("https://localhost/x"), "(no host)")
        self.assertEqual(up.host("https://xn--bcher-kva.example/p"), "xn--bcher-kva.example")


class TextWritesAmount(unittest.TestCase):
    """The extractor's own rule (`text_writes_amount` in tools/fetch.rs)."""

    def test_point_or_comma_but_not_a_fragment_of_a_longer_number(self):
        self.assertTrue(up.text_writes_amount("Nu voor € 12,99 per stuk", 12.99))
        self.assertTrue(up.text_writes_amount("Price: 12.99 EUR", 12.99))
        self.assertFalse(up.text_writes_amount("was 112,99", 12.99))
        self.assertFalse(up.text_writes_amount("12,995 sold", 12.99))
        self.assertFalse(up.text_writes_amount("nothing here", 12.99))

    def test_a_whole_amount_is_looked_for_with_its_cents(self):
        self.assertTrue(up.text_writes_amount("€ 349,00", 349))
        self.assertFalse(up.text_writes_amount("349 reviews", 349))

    def test_only_an_ascii_digit_makes_a_longer_number(self):
        # Rust's is_ascii_digit: a digit of another script beside the amount is not part of it.
        self.assertTrue(up.text_writes_amount("\u0663" + "12,99", 12.99))


class Measure(unittest.TestCase):
    def setUp(self):
        a, b, c, d = (f"https://shop-{x}.example/p/1" for x in "abcd")
        self.rows = [
            # Run 1: three shops compared.
            fetch(1, 0, a, page(price="10.00 EUR")),
            fetch(1, 1, b, page(product=False, text="Nu € 20,00 in de winkel")),
            fetch(1, 2, c, page(product=False, text="see the price in the shop")),
            fetch(1, 3, d, detail="page returned HTTP 403 Forbidden"),
            fetch(1, 4, a + "/", page(price="10.00 EUR")),  # the same page again: served from the run's own memory
            fetch(1, 5, "https://shop-a.example/p/6", detail="page budget spent (5 opens per request) — answer with what you already have"),
            compare(1, 6, [
                offer(a, 10.0, shipping=4.95),
                offer(b, 20.0),
                offer(c, 30.0),
                offer(d, 40.0),
                offer("https://market.example/listing/9", 5.0),  # never opened: the price came from a search
            ]),
            # Run 2: a product page read and not offered, and a page that is not a product.
            fetch(2, 0, "https://shop-a.example/p/2", page(price=None)),
            fetch(2, 1, "https://forum.example/thread", page(product=False, text="what people say")),
            compare(2, 2, [offer(a, 10.0)], status="failed"),  # a comparison that failed compared nothing
            # Run 3, another account: a product page priced and never offered.
            fetch(3, 0, "https://shop-a.example/p/3", page(price="7.00 EUR")),
        ]
        self.m = up.measure(self.rows, accounts={1: 10, 2: 10, 3: 20})

    def test_fetch_calls_are_counted_once_a_page_and_the_refused_apart(self):
        f = self.m["fetch"]
        self.assertEqual(f["calls"], 9)
        self.assertEqual(f["again"], 1)
        self.assertEqual(f[up.NOT_OPENED], 1)
        self.assertEqual(f["requested"], 7)
        self.assertEqual((f[up.PRICED], f[up.NO_PRICE], f[up.BLOCKED], f[up.GONE], f[up.FAILED]), (2, 4, 1, 0, 0))
        # One of the three runs had a page refused for its budget.
        self.assertEqual(f["runs"], 3)
        self.assertEqual(f["runs_refused"], 1)

    def test_a_page_refused_twice_is_two_refusals_not_a_page_served_again(self):
        refusal = "page budget spent (5 opens per request) — answer with what you already have"
        f = up.measure([
            fetch(1, 0, "https://shop.example/p", detail=refusal),
            fetch(1, 1, "https://shop.example/p", detail=refusal),
        ])["fetch"]
        self.assertEqual((f[up.NOT_OPENED], f["again"], f["requested"]), (2, 0, 0))

    def test_offers_are_counted_with_and_without_shipping(self):
        o = self.m["offers"]
        self.assertEqual(o["calls"], 1)
        self.assertEqual(o["offers"], 5)
        self.assertEqual(o["no_shipping"], 4)
        self.assertEqual(o["calls_stating_none"], 0)

    def test_an_offered_page_is_followed_back_to_how_it_was_read(self):
        o = self.m["offered"]
        self.assertEqual(o["never_opened"], 1)
        self.assertEqual(o[up.PRICED], 1)
        self.assertEqual(o[up.NO_PRICE], 2)
        self.assertEqual(o[up.BLOCKED], 1)
        # Of the two read with no price, one page wrote the offered amount.
        self.assertEqual(o["text_writes"], 1)

    def test_reading_the_page_is_set_beside_whether_the_offer_stated_shipping(self):
        o = self.m["offered"]
        # Read: a stated it, b and c did not. Never opened: the listing did not.
        self.assertEqual(o["read_no_shipping"], 2)
        self.assertEqual(o["never_opened_no_shipping"], 1)
        # The one that did state it stated a cost, not "free".
        self.assertEqual((o["read_shipping_stated"], o["read_shipping_free"]), (1, 0))
        # d was asked for and blocked; its offer stated none either.
        self.assertEqual((o["unread"], o["unread_no_shipping"]), (1, 1))

    def test_product_pages_are_the_offered_and_the_ones_whose_markup_named_a_product(self):
        p = self.m["product"]
        # Offered and read: a (priced), b and c (not). Markup only: run 2's page
        # (not priced) and run 3's (priced).
        self.assertEqual(p["read"], 5)
        self.assertEqual(p["no_price"], 3)
        self.assertEqual(p["offered_read"], 3)
        self.assertEqual(p["offered_no_price"], 2)
        self.assertEqual((p["markup_only"], p["markup_only_no_price"]), (2, 1))

    def test_hosts_are_ranked_by_what_fell_short_there(self):
        hosts = self.m["hosts"]
        ranked = [h for h, _ in up.ranked(hosts)]
        # b and c: an offer with no shipping and an offered page with no price, two each.
        # d: an offer with no shipping and a blocked read, two. market: one. a: none.
        self.assertEqual(ranked[:3], ["shop-b.example", "shop-c.example", "shop-d.example"])
        self.assertEqual(ranked[3], "market.example")
        self.assertEqual(up.shortfalls(hosts["shop-a.example"]), 0)
        self.assertEqual(up.shortfalls(hosts["shop-d.example"]), 2)
        self.assertEqual(hosts["shop-a.example"][up.PRICED], 2)
        self.assertEqual(hosts["shop-a.example"][up.NO_PRICE], 1)
        # How much stands behind a row: the runs it was seen in, and whose they were.
        self.assertEqual((hosts["shop-a.example"]["runs"], hosts["shop-a.example"]["accounts"]), (3, 2))
        self.assertEqual((hosts["shop-b.example"]["runs"], hosts["shop-b.example"]["accounts"]), (1, 1))
        # Where the offered page had no price: did its text write the amount?
        self.assertEqual(hosts["shop-b.example"]["offered_in_text"], 1)
        self.assertEqual(hosts["shop-c.example"]["offered_in_text"], 0)

    def test_the_rule_is_read_off_the_two_shares(self):
        r = up.rule(self.m)
        self.assertEqual(r["no_price_share"], 3 / 5)
        self.assertEqual(r["no_shipping_share"], 4 / 5)
        self.assertTrue(r["needed"])

    def test_nothing_measured_is_not_a_need(self):
        r = up.rule(up.measure([]))
        self.assertFalse(r["needed"])
        self.assertIsNone(r["no_price_share"])


def shares(no_price, read, no_shipping, offers):
    """A measurement holding only the four counts the rule reads."""
    import collections
    return {
        "product": collections.Counter(no_price=no_price, read=read),
        "offers": collections.Counter(no_shipping=no_shipping, offers=offers),
    }


class Rule(unittest.TestCase):
    """One product page in ten with no price, or one offer in three with no shipping."""

    def test_either_half_alone_is_enough(self):
        price_only = up.rule(shares(5, 10, 0, 30))
        self.assertEqual((price_only["price_met"], price_only["shipping_met"], price_only["needed"]), (True, False, True))
        shipping_only = up.rule(shares(0, 10, 20, 30))
        self.assertEqual((shipping_only["price_met"], shipping_only["shipping_met"], shipping_only["needed"]), (False, True, True))

    def test_neither_half_is_no_need(self):
        r = up.rule(shares(0, 100, 0, 100))
        self.assertEqual((r["price_met"], r["shipping_met"], r["needed"]), (False, False, False))

    def test_each_threshold_is_met_exactly_at_it_and_not_one_short(self):
        self.assertTrue(up.rule(shares(10, 100, 0, 9))["price_met"])
        self.assertFalse(up.rule(shares(9, 100, 0, 9))["price_met"])
        self.assertTrue(up.rule(shares(0, 9, 100, 300))["shipping_met"])
        self.assertFalse(up.rule(shares(0, 9, 99, 300))["shipping_met"])
        # Between the two thresholds, each keeps its own.
        self.assertTrue(up.rule(shares(2, 10, 0, 9))["price_met"])
        self.assertFalse(up.rule(shares(0, 9, 2, 10))["shipping_met"])

    def test_the_report_says_what_the_rule_found(self):
        def verdict(rows):
            text = up.report(up.measure(rows), top=5, window="a window")
            return [line.strip() for line in text.splitlines() if "the rule asks" in line or line.strip().startswith("needed:")]
        priced = [fetch(1, 0, "https://shop.example/p", page(price="1.00 EUR")),
                  compare(1, 1, [offer("https://shop.example/p", 1.0, shipping=0)])]
        price, shipping, needed = verdict(priced)
        self.assertTrue(price.endswith(": not met") and shipping.endswith(": not met"), (price, shipping))
        self.assertEqual(needed, "needed: no")
        unpriced = [fetch(1, 0, "https://shop.example/p", page(product=False)),
                    compare(1, 1, [offer("https://shop.example/p", 1.0, shipping=0)])]
        price, shipping, needed = verdict(unpriced)
        self.assertTrue(price.endswith(": met") and shipping.endswith(": not met"), (price, shipping))
        self.assertEqual(needed, "needed: yes")


class Repeats(unittest.TestCase):
    def test_a_page_asked_for_again_after_a_wall_counts_as_how_it_was_finally_read(self):
        url = "https://shop.example/p"
        m = up.measure([
            fetch(1, 0, url, detail="page returned HTTP 403 Forbidden"),
            fetch(1, 1, url, page(price="5.00 EUR")),
            compare(1, 2, [offer(url, 5.0)]),
        ])
        # Two requests were made, and both are counted; the page is the better of them.
        self.assertEqual((m["fetch"][up.BLOCKED], m["fetch"][up.PRICED], m["fetch"]["again"]), (1, 1, 0))
        self.assertEqual((m["offered"][up.PRICED], m["offered"][up.BLOCKED]), (1, 0))

    def test_a_later_failure_does_not_replace_an_earlier_one_that_stands_higher(self):
        url = "https://shop.example/p"
        m = up.measure([
            fetch(1, 0, url, detail="page returned HTTP 403 Forbidden"),
            fetch(1, 1, url, detail="page returned HTTP 404 Not Found"),
            compare(1, 2, [offer(url, 5.0)]),
        ])
        self.assertEqual((m["offered"][up.BLOCKED], m["offered"][up.GONE]), (1, 0))

    def test_calls_are_read_in_the_order_the_run_made_them_whatever_order_they_arrive_in(self):
        url = "https://shop.example/p"
        m = up.measure([
            fetch(1, 1, url, page(price="5.00 EUR")),
            fetch(1, 0, url, detail="page returned HTTP 403 Forbidden"),
        ])
        # The wall came first and the read second: two requests, neither a page served again.
        self.assertEqual((m["fetch"][up.BLOCKED], m["fetch"][up.PRICED], m["fetch"]["again"]), (1, 1, 0))

    def test_only_the_page_budget_counts_as_the_budget_refusing_a_run(self):
        f = up.measure([
            fetch(1, 0, "https://10.0.0.1.example/p", detail="that address is not on the public internet — it cannot be opened"),
        ])["fetch"]
        self.assertEqual((f[up.NOT_OPENED], f["runs_refused"]), (1, 0))

    def test_an_unread_page_whose_offer_stated_shipping_is_not_counted_as_stating_none(self):
        url = "https://shop.example/p"
        o = up.measure([
            fetch(1, 0, url, detail="page returned HTTP 403 Forbidden"),
            compare(1, 1, [offer(url, 5.0, shipping=3.0)]),
        ])["offered"]
        self.assertEqual((o["unread"], o["unread_no_shipping"]), (1, 0))

    def test_an_address_offered_twice_in_a_run_is_one_page_and_the_first_offer_speaks_for_it(self):
        url = "https://shop.example/p"
        m = up.measure([
            fetch(1, 0, url, page(product=False, text="nu 5,00")),
            compare(1, 1, [offer(url, 5.0)]),
            compare(1, 2, [offer(url, 9.0, shipping=3.0)]),
        ])
        self.assertEqual(m["offers"]["offers"], 2)           # every offer handed over
        self.assertEqual(m["offers"]["no_shipping"], 1)
        self.assertEqual(m["product"]["offered_read"], 1)    # one page
        self.assertEqual(m["offered"]["read_no_shipping"], 1)
        self.assertEqual(m["offered"]["text_writes"], 1)     # 5.00, the first offer's amount

    def test_an_offered_page_the_budget_refused_is_neither_read_nor_never_asked_for(self):
        url = "https://shop.example/p"
        m = up.measure([
            fetch(1, 0, url, detail="page budget spent (5 opens per request) — answer with what you already have"),
            compare(1, 1, [offer(url, 5.0)]),
        ])
        o = m["offered"]
        self.assertEqual((o[up.NOT_OPENED], o["never_opened"], o["unread"], o["unread_no_shipping"]), (1, 0, 1, 1))
        self.assertEqual(m["product"]["read"], 0)

    def test_a_missing_amount_past_the_cut_is_told_from_one_simply_missing(self):
        a, b = "https://shop.example/a", "https://shop.example/b"
        m = up.measure([
            fetch(1, 0, a, page(product=False, text="long page", truncated=True)),
            fetch(1, 1, b, page(product=False, text="short page")),
            compare(1, 2, [offer(a, 5.0), offer(b, 6.0)]),
        ])
        self.assertEqual((m["offered"]["text_writes"], m["offered"]["text_cut"]), (0, 1))

    def test_listings_found_through_an_api_are_set_apart_from_shops(self):
        m = up.measure([
            compare(1, 0, [
                offer("https://www.ebay.nl/itm/1", 5.0, shipping=4.0),
                offer("https://link.marktplaats.nl/m1", 5.0),
                offer("https://shop.example/p", 5.0),
            ]),
        ])
        o = m["offered"]
        self.assertEqual((o["never_opened"], o["never_opened_no_shipping"]), (3, 2))
        self.assertEqual((o["never_opened_listed"], o["never_opened_listed_no_shipping"]), (2, 1))
        # What is printed for shops is what is left once the listings are taken out.
        text = up.report(m, top=5, window="a window")
        self.assertIn("never asked for, a shop: 1 of 1 stated none", text)
        self.assertIn("never asked for, a listing found through an API: 1 of 2 stated none", text)

    def test_free_is_told_from_a_cost_where_the_page_was_never_asked_for_too(self):
        m = up.measure([
            compare(1, 0, [
                offer("https://a.example/p", 5.0, shipping=0),
                offer("https://b.example/p", 5.0, shipping=4.0),
                offer("https://c.example/p", 5.0),
                offer("https://www.ebay.nl/itm/1", 5.0, shipping=0),
            ]),
        ])
        o = m["offered"]
        self.assertEqual((o["never_opened_free"], o["never_opened_listed_free"]), (2, 1))
        self.assertIn("never asked for, a shop: 1 of 3 stated none (33%); of the 2 that stated it, 1 said free",
                      up.report(m, top=5, window="a window"))

    def test_free_shipping_is_stated_shipping(self):
        url = "https://shop.example/p"
        m = up.measure([fetch(1, 0, url, page(price="5.00 EUR")), compare(1, 1, [offer(url, 5.0, shipping=0)])])
        self.assertEqual(m["offers"]["no_shipping"], 0)
        self.assertEqual((m["offered"]["read_shipping_stated"], m["offered"]["read_shipping_free"]), (1, 1))


class Ranking(unittest.TestCase):
    def test_a_tie_goes_to_the_host_with_more_pages_asked_for_then_to_the_name(self):
        rows = [compare(1, 0, [offer(f"https://{h}.example/p", 1.0) for h in ("c", "a", "b")]),
                fetch(1, 1, "https://b.example/x", page(product=False)),
                fetch(1, 2, "https://b.example/y", page(product=False))]
        self.assertEqual([h for h, _ in up.ranked(up.measure(rows)["hosts"])], ["b.example", "a.example", "c.example"])


class Report(unittest.TestCase):
    def test_the_report_names_hosts_and_never_a_path_a_title_or_a_price(self):
        rows = [
            fetch(1, 0, "https://shop-a.example/secret-product-path?q=private", page(product=False, text="private words 77,77")),
            compare(1, 1, [dict(offer("https://shop-a.example/secret-product-path?q=private", 77.77), title="Private title")]),
        ]
        text = up.report(up.measure(rows), top=5, window="a window", min_accounts=1)
        self.assertIn("shop-a.example", text)
        for leak in ("secret-product-path", "private", "Private title", "77"):
            self.assertNotIn(leak, text)

    def test_a_host_only_one_account_used_is_counted_and_not_named(self):
        rows = [
            compare(1, 0, [offer("https://shared.example/p", 1.0), offer("https://one-person.example/p", 1.0)]),
            compare(2, 0, [offer("https://shared.example/p", 1.0)]),
            compare(3, 0, [offer("https://unknown-owner.example/p", 1.0)]),
        ]
        m = up.measure(rows, accounts={1: 10, 2: 20})  # run 3 belongs to nobody we know of
        text = up.report(m, top=5, window="a window")  # two accounts, unless told otherwise
        self.assertIn("shared.example", text)
        self.assertNotIn("one-person.example", text)
        self.assertNotIn("unknown-owner.example", text)
        self.assertIn("2 hosts fewer than 2 accounts used", text)
        # For the operator's own eyes, everything.
        self.assertIn("one-person.example", up.report(m, top=5, window="a window", min_accounts=1))

    def test_a_run_nobody_is_known_to_own_does_not_make_a_second_account(self):
        rows = [compare(1, 0, [offer("https://shop.example/p", 1.0)]), compare(2, 0, [offer("https://shop.example/p", 1.0)])]
        m = up.measure(rows, accounts={1: 10})
        self.assertEqual((m["hosts"]["shop.example"]["runs"], m["hosts"]["shop.example"]["accounts"]), (2, 1))
        self.assertNotIn("shop.example", up.report(m, top=5, window="a window"))

    def test_without_accounts_nothing_is_named(self):
        rows = [compare(1, 0, [offer("https://shop.example/p", 1.0)]), compare(2, 0, [offer("https://shop.example/p", 1.0)])]
        self.assertNotIn("shop.example", up.report(up.measure(rows), top=5, window="a window"))

    def test_hosts_past_the_top_are_summed_not_listed(self):
        rows = []
        for i in range(4):
            rows.append(compare(i, 0, [offer(f"https://shop-{i}.example/p", 1.0)] * (4 - i)))
        text = up.report(up.measure(rows), top=2, window="a window", min_accounts=1)
        self.assertIn("shop-0.example", text)
        self.assertIn("shop-1.example", text)
        self.assertNotIn("shop-2.example", text)
        # The two past the top: 2 + 1 offers, none stating shipping, so 3 short.
        rest = next(line for line in text.splitlines() if "2 other hosts" in line)
        self.assertEqual(rest.split("|")[1].split(), ["3", "3"])
        self.assertEqual(rest.split("|")[-1].split(), ["3"])


class Database(unittest.TestCase):
    def test_only_the_window_s_tool_rows_are_read_from_a_copy_of_the_database(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = str(pathlib.Path(tmp) / "scout.duckdb")
            con = duckdb.connect(path)
            con.execute(
                "CREATE TABLE run_traces (run_id BIGINT, seq BIGINT, kind TEXT, tool TEXT, args TEXT, "
                "nested BOOLEAN, started_at TIMESTAMP, duration_ms BIGINT, status TEXT, detail TEXT, result TEXT, truncated BOOLEAN)"
            )
            con.execute("CREATE TABLE runs (id BIGINT, account_id BIGINT)")
            con.execute("INSERT INTO runs VALUES (1, 10), (2, 10), (3, 10), (4, 20), (5, 20), (6, 20)")
            newest = datetime.datetime(2026, 11, 17, 12, 0, 0)
            def put(run, seq, kind, tool, url, at, status="ok"):
                con.execute(
                    "INSERT INTO run_traces VALUES (?, ?, ?, ?, ?, false, ?, 10, ?, NULL, ?, false)",
                    [run, seq, kind, tool, json.dumps({"url": url}), at, status, page(price="1.00 EUR")],
                )
            # Sixty days ending on the newest trace's day: 19 September to 17 November, whole days.
            put(1, 0, "tool", "fetch_page", "https://before.example/p", datetime.datetime(2026, 9, 18, 23, 59, 59))
            put(2, 0, "tool", "fetch_page", "https://first.example/p", datetime.datetime(2026, 9, 19, 0, 0, 0))
            put(3, 0, "tool", "search_web", "https://other.example/p", newest - datetime.timedelta(days=1))
            put(4, 0, "tool", "fetch_page", "https://newest.example/p", newest)
            put(5, 0, "event", None, "https://event.example/p", newest)
            put(6, 0, "tool", "fetch_page", "https://unfinished.example/p", newest, status=None)  # the run died mid-call
            con.close()
            rows, window = up.read(path, days=60)
            self.assertEqual(sorted(up.host(json.loads(r[3])["url"]) for r in rows), ["first.example", "newest.example"])
            self.assertEqual((window["since"], window["until"], window["runs"]), ("2026-09-19", "2026-11-17", 2))
            self.assertEqual(window["accounts"], {2: 10, 4: 20})

    def test_the_command_names_a_host_only_one_account_used_when_asked_and_not_before(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = str(pathlib.Path(tmp) / "scout.duckdb")
            con = duckdb.connect(path)
            con.execute(
                "CREATE TABLE run_traces (run_id BIGINT, seq BIGINT, kind TEXT, tool TEXT, args TEXT, "
                "nested BOOLEAN, started_at TIMESTAMP, duration_ms BIGINT, status TEXT, detail TEXT, result TEXT, truncated BOOLEAN)"
            )
            con.execute("CREATE TABLE runs (id BIGINT, account_id BIGINT)")
            con.execute("INSERT INTO runs VALUES (1, 10), (2, 20), (3, 10)")
            at = datetime.datetime(2026, 11, 17, 12, 0, 0)
            for run, url in ((1, "https://shared.example/p"), (2, "https://shared.example/p"), (3, "https://one-person.example/p")):
                con.execute(
                    "INSERT INTO run_traces VALUES (?, 0, 'tool', 'fetch_page', ?, false, ?, 10, 'ok', NULL, ?, false)",
                    [run, json.dumps({"url": url}), at, page(price="1.00 EUR")],
                )
            con.close()
            script = str(HERE / "unread-pages.py")
            run = lambda *flags: subprocess.run([sys.executable, script, path, *flags], capture_output=True, text=True, check=True).stdout
            plain = run()
            self.assertIn("shared.example", plain)
            self.assertNotIn("one-person.example", plain)
            self.assertIn("3 runs of 2 accounts", plain)
            self.assertIn("one-person.example", run("--min-accounts", "1"))
            refused = subprocess.run([sys.executable, script, path, "--days", "0"], capture_output=True, text=True)
            self.assertNotEqual(refused.returncode, 0)

    def test_a_database_that_begins_inside_the_window_says_when_it_begins(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = str(pathlib.Path(tmp) / "scout.duckdb")
            con = duckdb.connect(path)
            con.execute(
                "CREATE TABLE run_traces (run_id BIGINT, seq BIGINT, kind TEXT, tool TEXT, args TEXT, "
                "nested BOOLEAN, started_at TIMESTAMP, duration_ms BIGINT, status TEXT, detail TEXT, result TEXT, truncated BOOLEAN)"
            )
            con.execute("CREATE TABLE runs (id BIGINT, account_id BIGINT)")
            for day in (1, 9):
                con.execute(
                    "INSERT INTO run_traces VALUES (1, ?, 'tool', 'fetch_page', ?, false, ?, 10, 'ok', NULL, ?, false)",
                    [day, json.dumps({"url": "https://shop.example/p"}), datetime.datetime(2026, 11, day, 8, 0, 0), page()],
                )
            con.close()
            _, window = up.read(path, days=60)
            self.assertEqual((window["since"], window["until"]), ("2026-11-01", "2026-11-09"))


if __name__ == "__main__":
    unittest.main()
