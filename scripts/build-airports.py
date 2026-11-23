#!/usr/bin/env python3
"""Builds crates/scout-web/src/airports.json: the names the trip page shows
when a reader rests the pointer on an airport code.

From OurAirports' airports.csv, which is in the public domain
(https://ourairports.com/data/; the file is mirrored at
https://davidmegginson.github.io/ourairports-data/airports.csv). Download
it, then, from the repository's root:

    python3 scripts/build-airports.py airports.csv

Kept: every airport with a three-letter IATA code and scheduled service,
4,132 of the file's 86,383 rows on 2026-11-23, and the IATA codes for all
of a city's airports together (LON, NYC: 26 of them), which OurAirports
does not list and Scout's flight search takes. A code an airport holds is
never given to a city.

Each entry is [name, city, country]. The city is left empty where the name
already says it ("Vienna International Airport"), and the country is the
ISO 3166 code, which the page names in the reader's own language.
"""

import csv
import json
import re
import pathlib
import sys
import unicodedata

OUT = pathlib.Path(__file__).resolve().parent.parent / "crates" / "scout-web" / "src" / "airports.json"
SOURCE = "OurAirports (public domain), https://ourairports.com/data/"

# The IATA's metropolitan codes Scout is likely to meet: a city with more
# than one airport, searched as one.
CITIES = {
    "BJS": ("Beijing", "CN"), "BUE": ("Buenos Aires", "AR"), "BUH": ("Bucharest", "RO"),
    "CHI": ("Chicago", "US"), "JKT": ("Jakarta", "ID"), "LON": ("London", "GB"),
    "MIL": ("Milan", "IT"), "MOW": ("Moscow", "RU"), "NYC": ("New York", "US"),
    "OSA": ("Osaka", "JP"), "PAR": ("Paris", "FR"), "REK": ("Reykjavík", "IS"),
    "RIO": ("Rio de Janeiro", "BR"), "ROM": ("Rome", "IT"), "SAO": ("São Paulo", "BR"),
    "SEL": ("Seoul", "KR"), "STO": ("Stockholm", "SE"), "TYO": ("Tokyo", "JP"),
    "WAS": ("Washington", "US"), "YMQ": ("Montreal", "CA"), "YTO": ("Toronto", "CA"),
    "BHZ": ("Belo Horizonte", "BR"), "DTT": ("Detroit", "US"), "SPK": ("Sapporo", "JP"),
    "TCI": ("Tenerife", "ES"), "YEA": ("Edmonton", "CA"),
}


def plain(text):
    """Lower case without accents, for asking whether a name says a city:
    "İstanbul Airport" says "Istanbul"."""
    decomposed = unicodedata.normalize("NFD", text.casefold())
    return "".join(c for c in decomposed if not unicodedata.combining(c))


def says(name, town):
    """Whether `name` has `town` in it as whole words: "Changbaishan
    Airport" has "baishan" in it, and does not say Baishan."""
    words, wanted = re.findall(r"\w+", plain(name)), re.findall(r"\w+", plain(town))
    return bool(wanted) and any(words[i:i + len(wanted)] == wanted for i in range(len(words)))


def tidy(text):
    """Runs of spaces as one: "Amravati  Airport" is written so in the file."""
    return " ".join(text.split())


def entries(rows):
    """The table, by code, from airports.csv's rows."""
    table = {}
    for row in rows:
        code = row["iata_code"]
        if not (len(code) == 3 and code.isascii() and code.isalpha() and code.isupper()):
            continue
        if row["scheduled_service"] != "yes" or row["type"] == "closed":
            continue
        name, city = tidy(row["name"]), tidy(row["municipality"])
        # "Pendik, Istanbul": the first part is the town the name may say.
        if city and says(name, city.split(",")[0]):
            city = ""
        table[code] = [name, city, row["iso_country"]]
    for code, (city, country) in CITIES.items():
        table.setdefault(code, [f"All {city} airports", "", country])
    return table


def write(table, out):
    lines = [f"  {json.dumps(code)}: {json.dumps(entry, ensure_ascii=False)}" for code, entry in sorted(table.items())]
    body = ",\n".join(lines)
    out.write_text(f'{{"source": {json.dumps(SOURCE)},\n"airports": {{\n{body}\n}}}}\n', encoding="utf-8")


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    with open(sys.argv[1], encoding="utf-8", newline="") as source:
        table = entries(csv.DictReader(source))
    write(table, OUT)
    print(f"{len(table)} codes written to {OUT}")


if __name__ == "__main__":
    main()
