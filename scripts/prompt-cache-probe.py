#!/usr/bin/env python3
"""Does the model provider cache a shared prompt prefix? Seven requests of
about 6,000 tokens each, counts only; the key is read from the environment
and never printed.

    LLM_API_KEY=... LLM_BASE_URL=https://api.minimax.io/v1 LLM_MODEL=minimax-m3 \
        scripts/prompt-cache-probe.py

What to look for: a provider with a prefix cache reports most of the prompt
as cached whenever the first thousands of tokens repeat, however the tail
differs (rows A2, B2, C2). One that caches only exact repeats reports a hit
on A3 and C3 alone. MiniMax M3, 2026-10-17: exact repeats only — 6000 of
6001 on a repeat, 128 on every prefix match — so where per-user text sits in
the prompt changes nothing there (docs/2026-10-17-prompt-cache.md).
"""
import json
import os
import sys
import time
import urllib.request

KEY = os.environ.get("LLM_API_KEY") or os.environ.get("MINIMAX_API_KEY")
BASE = (os.environ.get("LLM_BASE_URL") or os.environ.get("MINIMAX_BASE_URL") or "https://api.minimax.io/v1").rstrip("/")
MODEL = os.environ.get("LLM_MODEL") or "minimax-m3"
if not KEY:
    sys.exit("set LLM_API_KEY (or MINIMAX_API_KEY)")

RULE = "Rule {}: compare landed cost, never sticker price; verify every link; refuse to sum mixed currencies; say when a price is a quote. "
RULES = "You are Scout, a careful shopping and travel assistant. " + "".join(RULE.format(i) for i in range(1, 120))
TOOLS = [
    {"type": "function", "function": {
        "name": name,
        "description": "Tool %s: %s" % (name, "searches the web for products, shops, prices and reviews and returns titles, urls and snippets " * 4),
        "parameters": {"type": "object", "properties": {"query": {"type": "string", "description": "the query, in the user's language"}, "limit": {"type": "integer"}}, "required": ["query"]},
    }}
    for name in ["search_web", "fetch_page", "search_secondhand", "compare_prices", "ask_flights"]
]
QUESTION = "Which laundry detergent is cheapest per wash?"


def facts(name):
    return ("\n\nKnown about this user (long-term profile):\n- name: %s\n- country: NL\n- sizes: 42\n\n"
            "Search languages for this user: Dutch, English.\n\nToday's date is 2026-10-17 (UTC).\n" % name)


def call(system, user):
    body = {"model": MODEL, "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}], "tools": TOOLS, "max_tokens": 8}
    req = urllib.request.Request(BASE + "/chat/completions", json.dumps(body).encode(), {"Authorization": "Bearer " + KEY, "Content-Type": "application/json"})
    usage = json.load(urllib.request.urlopen(req, timeout=120)).get("usage") or {}
    details = usage.get("prompt_tokens_details") or {}
    return "prompt %s, cached %s" % (usage.get("prompt_tokens"), details.get("cached_tokens", details.get("cached_input_tokens")))


def main():
    rows = [
        ("A1 facts last in the system prompt, user 1", RULES + facts("Anna"), QUESTION),
        ("A2 same, user 2 (prefix shared, tail differs)", RULES + facts("Boris"), QUESTION),
        ("A3 user 2 again (exact repeat)", RULES + facts("Boris"), QUESTION),
        ("B1 facts in the user message, user 3", RULES, QUESTION + facts("Clara")),
        ("B2 same, user 4 (prefix shared, tail differs)", RULES, QUESTION + facts("Dmitri")),
        ("C2 user 1 with one sentence appended", RULES + facts("Anna"), QUESTION + " Please be brief."),
        ("C3 that again (exact repeat)", RULES + facts("Anna"), QUESTION + " Please be brief."),
    ]
    for label, system, user in rows:
        print(f"{label:48} {call(system, user)}")
        time.sleep(2)


if __name__ == "__main__":
    main()
