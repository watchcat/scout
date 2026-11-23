#!/usr/bin/env bash
# Tests for scripts/shipped.sh. Run: scripts/shipped.test.sh
set -uo pipefail
cd "$(dirname "$0")"
. ./shipped.sh

fails=0
page() {  # a metrics page naming $1, among the lines around it
    printf '# HELP scout_build_info Always 1.\n# TYPE scout_build_info gauge\nscout_build_info{commit="%s"} 1\n# TYPE scout_runs_total counter\n' "$1"
}

says() {  # name, page, commit, text the reason must contain ("" when it must pass)
    local out status
    out=$(shipped_problem "$2" "$3"); status=$?
    if [ -z "$4" ]; then
        if [ "$status" -ne 0 ] || [ -n "$out" ]; then echo "FAIL $1: status $status: $out"; fails=$((fails + 1)); else echo "ok   $1"; fi
    elif [ "$status" -ne 1 ]; then
        echo "FAIL $1: status $status, wanted 1: $out"; fails=$((fails + 1))
    elif [[ "$out" != *"$4"* ]]; then
        echo "FAIL $1: reason does not mention '$4': $out"; fails=$((fails + 1))
    else echo "ok   $1"; fi
}

says "the commit the deploy was named for"        "$(page 486ec6a)" 486ec6a ""
# What the stale build shipped: the binary of the build before.
says "another commit"                              "$(page 93a4da9)" 486ec6a "93a4da9, not 486ec6a"
says "a longer name that begins the same"          "$(page 486ec6af)" 486ec6a "486ec6af, not 486ec6a"
says "a shorter one the deploy's begins with"      "$(page 486ec6)" 486ec6a "486ec6, not 486ec6a"
# Built without the commit: by hand, or with the name lost on its way into
# the build. Either is what the check is for.
says "a binary that names no commit"               "$(page '')" 486ec6a "names no commit"
says "a page from before the binary named one"     '# TYPE scout_runs_total counter' 486ec6a "no scout_build_info"
says "no page at all"                              "" 486ec6a "no scout_build_info"
long="<html>$(printf '%*s' 400 '' | tr ' ' x)</html>"
out=$(shipped_problem "$long" 486ec6a)
if [ "${#out}" -gt 200 ]; then echo "FAIL a page of something else: ${#out} characters"; fails=$((fails + 1))
else echo "ok   a page of something else is cut short"; fi

# shipped_check, with a stand-in for asking the pod that counts its calls.
calls=$(mktemp)
answers() { echo x >> "$calls"; page "$ANSWER"; }
refused() { echo x >> "$calls"; echo "curl: (7) Failed to connect to 10.42.0.9 port 9090" >&2; return 7; }
late() { echo x >> "$calls"; if [ "$(wc -l < "$calls")" -lt 2 ]; then return 7; fi; page 486ec6a; }

checks() {  # name, fetch, status wanted, calls wanted, text the reason must contain
    local out status n
    : > "$calls"
    out=$(shipped_check 486ec6a "$2" 3 0); status=$?
    n=$(wc -l < "$calls" | tr -d ' ')
    if [ "$status" -ne "$3" ] || [ "$n" -ne "$4" ]; then
        echo "FAIL $1: status $status after $n asks, wanted $3 after $4: $out"; fails=$((fails + 1))
    elif [[ "$out" != *"$5"* ]]; then
        echo "FAIL $1: reason does not mention '$5': $out"; fails=$((fails + 1))
    else echo "ok   $1"; fi
}
ANSWER=486ec6a checks "the pod runs it: asked once"                answers 0 1 ""
# A definite answer is not asked for again: it will not change.
ANSWER=93a4da9 checks "the pod runs another: asked once, failed"   answers 1 1 "93a4da9, not 486ec6a"
checks "the pod cannot be asked: three times, then said"           refused 2 3 "Failed to connect"
checks "the pod answers the second time"                           late 0 2 ""
rm -f "$calls"

[ "$fails" -eq 0 ] && echo "all passed" || { echo "$fails failed"; exit 1; }
