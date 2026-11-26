#!/usr/bin/env python3
"""Tests for flight-cost.py: python3 scripts/flight-cost.test.py

Every run here is written out, so each number the script gives is one a
reader can count by hand. Needs the `duckdb` package, as the script does.
"""
import importlib.util
import json
import pathlib
import tempfile
import unittest

import duckdb

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("flight_cost", HERE / "flight-cost.py")
fc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fc)

SCHEMA = [
    "create table runs (id bigint, account_id bigint, conversation_id bigint, started_at timestamp, "
    "ended_at timestamp, outcome varchar, detail varchar, input_tokens bigint, cached_tokens bigint, output_tokens bigint)",
    "create table run_traces (id bigint, run_id bigint, seq bigint, kind varchar, tool varchar, args varchar, "
    "nested boolean, started_at timestamp, duration_ms bigint, status varchar, detail varchar, result varchar, truncated boolean)",
    "create table request_log (account_id bigint, kind varchar, created_at timestamp)",
]


class Db:
    def __init__(self):
        self.dir = tempfile.TemporaryDirectory()
        self.c = duckdb.connect(str(pathlib.Path(self.dir.name) / "t.duckdb"))
        for statement in SCHEMA:
            self.c.execute(statement)
        self.seq = 0

    def run(self, rid, start, end, tokens=(100_000, 60_000, 9_000), account=7):
        self.c.execute("insert into runs values (?, ?, 1, ?, ?, 'answered', null, ?, ?, ?)",
                       [rid, account, start, end, *tokens])
        self.call(rid, "ask_flights", {"brief": "a brief"})

    def call(self, rid, tool, args, status="ok", result="{}", truncated=False):
        self.seq += 1
        self.c.execute("insert into run_traces values (?, ?, ?, 'tool', ?, ?, ?, null, 5, ?, null, ?, ?)",
                       [self.seq, rid, self.seq, tool, json.dumps(args), tool != "ask_flights", status, result, truncated])

    def bought(self, at, n, account=7):
        for i in range(n):
            self.c.execute("insert into request_log values (?, 'flight_search', ?::timestamp + ?::interval)",
                           [account, at, f"{i} seconds"])


def finalise(flights, booked=0):
    return json.dumps({"segments": [{"booking": [{"url": "x"}] if i < booked else []} for i in range(flights)]})


class MeasureTest(unittest.TestCase):
    def test_a_flexible_return_is_priced_from_its_tokens_and_the_days_it_bought(self):
        db = Db()
        db.run(1, "2026-11-24 10:00:00", "2026-11-24 10:02:00", tokens=(99_622, 66_044, 9_387))
        db.call(1, "search_flights", {"departure_date": "2026-12-01", "return_date": "2026-12-08", "flex_days": 2})
        db.bought("2026-11-24 10:00:20", 8)
        [r], skipped = fc.measure(db.c)
        self.assertEqual((r["bought"], r["flex"], r["ret"], skipped), (8, 2, True, 0))
        self.assertAlmostEqual(r["model"], 0.0253, places=4)
        self.assertAlmostEqual(r["providers"], 8 * 0.007, places=6)

    def test_days_bought_by_the_same_account_outside_the_run_are_not_its(self):
        db = Db()
        db.run(1, "2026-11-24 10:00:00", "2026-11-24 10:01:00")
        db.run(2, "2026-11-24 10:01:03", "2026-11-24 10:02:00")
        db.bought("2026-11-24 10:00:30", 4)
        db.bought("2026-11-24 10:01:10", 4)
        rows, _ = fc.measure(db.c)
        self.assertEqual([r["bought"] for r in rows], [4, 4])

    def test_a_finalise_is_a_request_at_each_provider_per_flight_and_one_multi_city(self):
        db = Db()
        db.run(1, "2026-11-24 10:00:00", "2026-11-24 10:02:00")
        db.call(1, "finalise_trip", {"trip": "t"}, result=finalise(3, booked=2))
        [r], _ = fc.measure(db.c)
        self.assertEqual((r["repriced"], r["multi"], r["lookups"]), (3, 1, 2))
        # Duffel: 3 flights + 1 multi-city; Ignav: 3 flights + 2 lookups.
        self.assertAlmostEqual(r["providers"], 4 * 0.005 + 5 * 0.002, places=6)

    def test_one_flight_to_finalise_has_no_multi_city_request(self):
        db = Db()
        db.run(1, "2026-11-24 10:00:00", "2026-11-24 10:02:00")
        db.call(1, "finalise_trip", {"trip": "t"}, result=finalise(1))
        [r], _ = fc.measure(db.c)
        self.assertEqual(r["multi"], 0)
        self.assertAlmostEqual(r["providers"], 0.007, places=6)

    def test_refused_calls_buy_nothing_and_set_no_window(self):
        db = Db()
        db.run(1, "2026-11-24 10:00:00", "2026-11-24 10:02:00")
        db.call(1, "search_flights", {"departure_date": "2026-12-01", "flex_days": 7}, status="failed", result="refused")
        db.call(1, "flight_booking_links", {"ignav_id": "x"}, status="failed", result="not shown")
        db.call(1, "finalise_trip", {"trip": "t"}, status="failed", result="not ready")
        [r], _ = fc.measure(db.c)
        self.assertEqual((r["calls"], r["flex"], r["lookups"], r["repriced"]), (1, 0, 0, 0))

    def test_a_cut_finalise_result_is_said_not_counted_as_nothing(self):
        db = Db()
        db.run(1, "2026-11-24 10:00:00", "2026-11-24 10:02:00")
        db.call(1, "finalise_trip", {"trip": "t"}, result='{"segments": [', truncated=True)
        [r], _ = fc.measure(db.c)
        self.assertEqual(r["unread_finalise"], 1)

    def test_a_run_with_no_end_is_skipped_and_counted(self):
        db = Db()
        db.run(1, "2026-11-24 10:00:00", None)
        rows, skipped = fc.measure(db.c)
        self.assertEqual((rows, skipped), ([], 1))

    def test_an_empty_database_says_so(self):
        db = Db()
        db.c.close()
        fc.main(str(pathlib.Path(db.dir.name) / "t.duckdb"))


if __name__ == "__main__":
    unittest.main()
