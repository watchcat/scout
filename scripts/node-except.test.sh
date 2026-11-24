#!/usr/bin/env bash
# Tests for scripts/node-except.sh. Run: scripts/node-except.test.sh
# The addresses are the ones set aside for documentation (RFC 5737).
set -uo pipefail
cd "$(dirname "$0")"
. ./node-except.sh

fails=0
says() {  # name, listing, InternalIP, what it must print, and 0 or 1
    local out status
    out=$(node_except "$2" "$3"); status=$?
    if [ "$status" -ne "$5" ]; then
        echo "FAIL $1: status $status, wanted $5: $out"; fails=$((fails + 1))
    elif [[ "$out" != *"$4"* ]]; then
        echo "FAIL $1: printed '$out', wanted '$4'"; fails=$((fails + 1))
    else echo "ok   $1"; fi
}

# As `ip -4 -o addr show scope global` lists them on a k3s node.
eth0='2: eth0    inet 203.0.113.4/32 metric 100 scope global dynamic eth0\       valid_lft 85000sec preferred_lft 85000sec'
docker='3: docker0    inet 172.17.0.1/16 brd 172.17.255.255 scope global docker0\       valid_lft forever preferred_lft forever'
flannel='4: flannel.1    inet 10.42.0.0/32 scope global flannel.1\       valid_lft forever preferred_lft forever'
cni='5: cni0    inet 10.42.0.1/24 brd 10.42.0.255 scope global cni0\       valid_lft forever preferred_lft forever'
floating='2: eth0    inet 198.51.100.7/32 scope global eth0:1\       valid_lft forever preferred_lft forever'

says "every address the node has, each a /32" "$eth0
$docker
$flannel
$cni" 203.0.113.4 "203.0.113.4/32, 172.17.0.1/32, 10.42.0.0/32, 10.42.0.1/32" 0
says "a second public one, as a floating address would be" "$eth0
$floating" 203.0.113.4 "203.0.113.4/32, 198.51.100.7/32" 0
says "exactly the list, and nothing after it" "$eth0" 203.0.113.4 "203.0.113.4/32" 0
out=$(node_except "$eth0" 203.0.113.4)
if [ "$out" != "203.0.113.4/32" ]; then echo "FAIL the one address alone: '$out'"; fails=$((fails + 1)); else echo "ok   the one address alone"; fi

# As the cluster says a dual-stack node's InternalIPs: both, with a space.
says "an InternalIP with an IPv6 one beside it" "$eth0
$cni" "203.0.113.4 2001:db8::4" "203.0.113.4/32, 10.42.0.1/32" 0
says "only an IPv6 InternalIP" "$eth0" "2001:db8::4" "InternalIP could not be read" 1
says "a listing without the InternalIP" "$docker
$cni" 203.0.113.4 "InternalIP is not among" 1
says "an InternalIP that only begins one of them" "$eth0" 203.0.113.40 "InternalIP is not among" 1
says "no InternalIP read" "$eth0" "" "InternalIP could not be read" 1
says "nothing listed" "" 203.0.113.4 "listed no address" 1
says "an IPv6 line, which this fence has no list for" "2: eth0    inet6 2001:db8::1/64 scope global" 203.0.113.4 "not an IPv4" 1
says "a line with no address" "2: eth0    inet garbage scope global" 203.0.113.4 "no IPv4 address" 1

if [ "$fails" -eq 0 ]; then echo "node-except: all passed"; else echo "node-except: $fails failed"; exit 1; fi
