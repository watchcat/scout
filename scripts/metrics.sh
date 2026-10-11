#!/usr/bin/env bash
# Print the running pod's /metrics.
#
# The metrics port is in neither the Service nor the Ingress, so it is asked
# from the node, where pod addresses are reachable, rather than from here.
#
#   scripts/metrics.sh                 the whole page
#   scripts/metrics.sh scout_runs      only the lines that mention it
set -euo pipefail
cd "$(dirname "$0")/.."

# Read from .env the way deploy-k3s.sh does: .env cannot be `source`d.
env_file_value() {
    [ -f .env ] || return 0
    sed -n "s/^$1=//p" .env | head -1
}
SCOUT_SSH="${SCOUT_SSH:-$(env_file_value SCOUT_SSH)}"
[ -n "$SCOUT_SSH" ] || { echo "SCOUT_SSH is not set and not in .env" >&2; exit 1; }

# Single-quoted: expanded on the node, not here.
page=$(ssh -o BatchMode=yes "$SCOUT_SSH" 'ip=$(kubectl -n scout get pod -l app=scout -o jsonpath="{.items[0].status.podIP}") && curl -fsS "http://$ip:9090/metrics"')
if [ -n "${1:-}" ]; then
    grep -F -- "$1" <<<"$page"
else
    printf '%s\n' "$page"
fi
