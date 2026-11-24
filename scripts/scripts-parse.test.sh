#!/usr/bin/env bash
# Every shell script here parses, and so does every script one of them
# hands to `bash -s` on another machine. Run: scripts/scripts-parse.test.sh
#
# A deploy script that does not parse fails at the moment it is needed,
# halfway through a deploy: one did, with an apostrophe inside `${…:?…}`,
# which bash reads as the start of a quoted string. And `bash -n` on a
# script does not parse what it sends elsewhere in a quoted heredoc, which
# is only text to it.
set -uo pipefail
cd "$(dirname "$0")"
fails=0
parses() {  # name, file
    local out
    if out=$(bash -n "$2" 2>&1); then
        echo "ok   $1"
    else
        echo "FAIL $1: $out"
        fails=$((fails + 1))
    fi
}
sent=$(mktemp -d)
trap 'rm -r "$sent"' EXIT
for script in ./*.sh; do
    parses "$script" "$script"
    # Each `bash -s … <<'NAME'` heredoc, from the line after it to NAME.
    while IFS= read -r name; do
        awk -v name="$name" '
            index($0, "<<'\''" name "'\''") && /bash -s/ { inside = 1; next }
            inside && $0 == name { exit }
            inside { print }
        ' "$script" > "$sent/$name"
        if [ ! -s "$sent/$name" ]; then
            echo "FAIL $script, its $name: nothing found to parse"
            fails=$((fails + 1))
        else
            parses "$script, what it sends as $name" "$sent/$name"
        fi
    done < <(grep -E "bash -s .*<<'[A-Z_]+'\$" "$script" | sed -E "s/.*<<'([A-Z_]+)'\$/\\1/")
done
if [ "$fails" -eq 0 ]; then echo "scripts-parse: all passed"; else echo "scripts-parse: $fails failed"; exit 1; fi
