#!/usr/bin/env bash
# Every shell script here parses. Run: scripts/scripts-parse.test.sh
#
# A deploy script that does not parse fails at the moment it is needed,
# halfway through a deploy: one did, with an apostrophe inside `${…:?…}`,
# which bash reads as the start of a quoted string.
set -uo pipefail
cd "$(dirname "$0")"
fails=0
for script in ./*.sh; do
    if out=$(bash -n "$script" 2>&1); then
        echo "ok   $script"
    else
        echo "FAIL $script: $out"
        fails=$((fails + 1))
    fi
done
if [ "$fails" -eq 0 ]; then echo "scripts-parse: all passed"; else echo "scripts-parse: $fails failed"; exit 1; fi
