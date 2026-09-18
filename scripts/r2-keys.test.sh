#!/usr/bin/env bash
# Tests for scripts/r2-keys.sh. Run: scripts/r2-keys.test.sh
set -uo pipefail
cd "$(dirname "$0")"
. ./r2-keys.sh

fails=0
hex() { printf '%*s' "$1" '' | tr ' ' 'a'; }

ok() {  # name, access, secret
    if out=$(r2_keys_problem "$2" "$3"); then echo "ok   $1"
    else echo "FAIL $1: rejected a good pair: $out"; fails=$((fails + 1)); fi
}
bad() {  # name, access, secret, text the reason must contain
    if out=$(r2_keys_problem "$2" "$3"); then
        echo "FAIL $1: accepted"; fails=$((fails + 1))
    elif [[ "$out" != *"$4"* ]]; then
        echo "FAIL $1: reason does not mention '$4': $out"; fails=$((fails + 1))
    else echo "ok   $1"; fi
}

ok  "the pair the R2 dashboard shows"          "$(hex 32)" "$(hex 64)"
ok  "upper-case hex is still hex"              "$(hex 32 | tr a A)" "$(hex 64 | tr a A)"
# What .env held for nine nights of failed backups: the token's value where
# its id belongs, and the account id where the secret belongs.
bad "a Cloudflare API token value as the id"   "cfut_$(hex 48)" "$(hex 64)" "token value"
bad "the account id as the secret"             "$(hex 32)" "$(hex 32)" "account ID"
bad "an id of the wrong length"                "$(hex 20)" "$(hex 64)" "AWS_ACCESS_KEY_ID"
bad "a secret that is not hex"                 "$(hex 32)" "$(hex 63)z" "AWS_SECRET_ACCESS_KEY"
bad "an empty id"                              "" "$(hex 64)" "AWS_ACCESS_KEY_ID"
# Never echo the value back: the reason ends up in a terminal and in CI logs.
if out=$(r2_keys_problem "cfut_$(hex 48)" "$(hex 32)"); then :; fi
if [[ "$out" == *"cfut_aaaa"* || "$out" == *"$(hex 32)"* ]]; then
    echo "FAIL the reason prints a key"; fails=$((fails + 1))
else echo "ok   the reason never prints a key"; fi

[ "$fails" -eq 0 ] && echo "all passed" || { echo "$fails failed"; exit 1; }
