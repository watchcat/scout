#!/usr/bin/env python3
"""Tests for scripts/build-airports.py. Run: python3 scripts/build-airports.test.py"""

import importlib.util
import json
import pathlib
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("build_airports", HERE / "build-airports.py")
build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build)


def row(code, name, city, country="AT", kind="large_airport", scheduled="yes"):
    return {"iata_code": code, "name": name, "municipality": city, "iso_country": country,
            "type": kind, "scheduled_service": scheduled}


class Entries(unittest.TestCase):
    def test_an_airport_with_scheduled_flights_is_kept_by_its_code(self):
        table = build.entries([row("EVN", "Zvartnots International Airport", "Yerevan", "AM")])
        self.assertEqual(table["EVN"], ["Zvartnots International Airport", "Yerevan", "AM"])

    def test_what_has_no_scheduled_flight_or_no_code_is_left_out(self):
        table = build.entries([
            row("ZZA", "A Strip", "Somewhere", scheduled="no"),
            row("ZZB", "A Closed Field", "Somewhere", kind="closed"),
            row("", "No Code Field", "Somewhere"),
            row("Z1Z", "Odd Code Field", "Somewhere"),
        ])
        self.assertEqual([code for code in table if code.startswith("Z")], [])

    def test_the_city_is_left_out_where_the_name_says_it(self):
        table = build.entries([
            row("VIE", "Vienna International Airport", "Vienna"),
            # Accents and the dotted capital I count as the letters they are.
            row("IST", "İstanbul Airport", "Istanbul", "TR"),
            # The first part of "town, region" is the town.
            row("SAW", "Istanbul Sabiha Gökçen International Airport", "Pendik, Istanbul", "TR"),
        ])
        self.assertEqual(table["VIE"][1], "")
        self.assertEqual(table["IST"][1], "")
        self.assertEqual(table["SAW"][1], "Pendik, Istanbul")

    def test_a_city_is_said_by_whole_words_only(self):
        table = build.entries([row("NBS", "Changbaishan Airport", "Baishan", "CN")])
        self.assertEqual(table["NBS"], ["Changbaishan Airport", "Baishan", "CN"])

    def test_runs_of_spaces_are_one(self):
        table = build.entries([row("AVR", "Amravati  Airport", " Amravati ", "IN")])
        self.assertEqual(table["AVR"], ["Amravati Airport", "", "IN"])

    def test_a_city_code_never_takes_an_airports(self):
        table = build.entries([row("LON", "A Real Airport Called LON", "Elsewhere", "US")])
        self.assertEqual(table["LON"][0], "A Real Airport Called LON")
        self.assertEqual(table["NYC"], ["All New York airports", "", "US"])

    def test_the_table_is_written_as_json_with_its_source(self):
        with tempfile.TemporaryDirectory() as folder:
            out = pathlib.Path(folder) / "airports.json"
            build.write({"VIE": ["Vienna International Airport", "", "AT"]}, out)
            written = json.loads(out.read_text(encoding="utf-8"))
        self.assertIn("public domain", written["source"])
        self.assertEqual(written["airports"], {"VIE": ["Vienna International Airport", "", "AT"]})


if __name__ == "__main__":
    unittest.main()
