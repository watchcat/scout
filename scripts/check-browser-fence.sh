#!/usr/bin/env bash
# The browser pod's fence, proved.
#
# A NetworkPolicy that is not enforced looks exactly like one that is, until
# the day it is needed. So each thing the fence must keep out is asked for
# from inside the browser pod, and the same thing is asked from a pod that
# no policy holds, started for the purpose from the same image: what the
# browser pod cannot reach counts as fenced only when that control could.
# A probe that can never succeed proves nothing.
#
# Run by scripts/deploy-k3s.sh after both rollouts, and by hand at any time:
#
#   scripts/check-browser-fence.sh <image tag>     e.g. 486ec6a; the control is started from scout:<tag>
#   scripts/check-browser-fence.sh --dry-run       what would be asked, asking nothing
#
# It runs on the node over SSH, as the deploy does, and prints no address:
# the node's and the pods' are written <node> and <scout pod>.
#
# The order of the deploy does not make an open fence dangerous for a
# moment: before this pod existed, every render ran in Scout's own pod,
# which reaches all of this and holds Scout's keys besides.
set -euo pipefail
cd "$(dirname "$0")/.."

if [ "${1:-}" = "--dry-run" ]; then
    cat <<'PLAN'
  (dry run) in the browser pod, each with a control in an unfenced pod:
    closed: Scout's Service (scout:80), Scout's metrics (<scout pod>:9090),
            the Kubernetes API (<node>:6443), the node's SSH (<node>:22),
            the cloud's metadata service (169.254.169.254:80),
            a datagram out (udp 1.1.1.1:53)
    open:   a name resolved (dns example.com), a page's server (tcp example.com:443),
            a render in the pod itself (render https://example.com/)
  (dry run) in: from Scout's pod a render through the Service; from the unfenced pod, nothing
  (dry run) held: no key, token, secret or password in the pod's environment,
            no service-account token, a root it cannot write
PLAN
    exit 0
fi

SHA=${1:?usage: scripts/check-browser-fence.sh <image tag> | --dry-run}
if [ -z "${SCOUT_SSH:-}" ] && [ -f .env ]; then
    SCOUT_SSH=$(sed -n 's/^SCOUT_SSH=//p' .env | head -1)
fi
: "${SCOUT_SSH:?set SCOUT_SSH to the node — in .env or the environment}"

ssh -o BatchMode=yes "$SCOUT_SSH" bash -s -- "$SHA" <<'ON_THE_NODE'
set -uo pipefail
SHA=$1
NS=scout
CONTROL=scout-fence-control
# The node's IPv4 InternalIP: the cluster lists an IPv6 one beside it, and
# the pods have no IPv6 to reach that with.
NODE=$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}' | tr ' ' '\n' | grep -E '^[0-9]+(\.[0-9]+){3}$' | head -1)
SCOUT_POD=$(kubectl -n "$NS" get pod -l app=scout --field-selector=status.phase=Running -o jsonpath='{.items[0].status.podIP}')
if [ -z "$NODE" ] || [ -z "$SCOUT_POD" ]; then
    echo "  the node's address or Scout's pod could not be read" >&2
    exit 1
fi
failed=0

# Every address said, written by what it is.
masked() { sed -e "s/${NODE//./\\.}/<node>/g" -e "s/${SCOUT_POD//./\\.}/<scout pod>/g"; }

in_browser() { kubectl -n "$NS" exec deploy/scout-browser -- scout-browser probe "$@" 2>&1; }
in_control() { kubectl -n "$NS" exec "$CONTROL" -- scout-browser probe "$@" 2>&1; }

# The control: the same image, no label any policy selects, no token.
kubectl -n "$NS" delete pod "$CONTROL" --ignore-not-found --wait=true >/dev/null
trap 'kubectl -n "$NS" delete pod "$CONTROL" --ignore-not-found --wait=false >/dev/null 2>&1' EXIT
kubectl -n "$NS" run "$CONTROL" --image="scout:$SHA" --image-pull-policy=IfNotPresent --restart=Never \
    --labels=role=fence-control --overrides='{"spec":{"automountServiceAccountToken":false}}' \
    --command -- sleep 600 >/dev/null
if ! kubectl -n "$NS" wait --for=condition=Ready "pod/$CONTROL" --timeout=90s >/dev/null; then
    echo "  the control pod did not start" >&2
    kubectl -n "$NS" describe pod "$CONTROL" | tail -15 >&2
    exit 1
fi

# What the browser pod must not reach. `strict`: the control must reach it,
# or the check proves nothing and fails. `lenient`: an unreached control is
# said, and the check is not counted either way.
closed() {
    local how=$1 what=$2 out
    shift 2
    if out=$(in_browser "$@"); then
        echo "  OPEN    $what: the browser pod reached it: $(masked <<<"$out")"
        failed=1
    elif in_control "$@" >/dev/null; then
        echo "  closed  $what"
    elif [ "$how" = lenient ]; then
        echo "  closed  $what (unproved: the control could not reach it either)"
    else
        echo "  UNPROVED $what: the control could not reach it either: $(masked <<<"$(in_control "$@")")"
        failed=1
    fi
}

# What the browser pod must reach.
open() {
    local what=$1 out
    shift
    if out=$(in_browser "$@"); then
        echo "  open    $what"
    else
        echo "  SHUT    $what: $(masked <<<"$out")"
        failed=1
    fi
}

echo "  out of the browser pod"
closed strict  "Scout's Service, scout:80" tcp scout:80
closed strict  "Scout's metrics, <scout pod>:9090" tcp "$SCOUT_POD:9090"
closed strict  "the Kubernetes API, <node>:6443" tcp "$NODE:6443"
closed strict  "the node's SSH, <node>:22" tcp "$NODE:22"
closed lenient "the cloud's metadata service, 169.254.169.254:80" tcp 169.254.169.254:80
closed strict  "a datagram out, a DNS question to 1.1.1.1:53" udp 1.1.1.1:53
open "a name resolved, example.com" dns example.com
open "a page's server, example.com:443" tcp example.com:443
# The pod's own render, asked of itself on loopback: Chromium starts and
# renders under this fence, this root and this user.
if out=$(kubectl -n "$NS" exec deploy/scout-browser -- env SCOUT_BROWSER_URL=http://127.0.0.1:8091 scout-browser probe render https://example.com/ 2>&1) \
    && [[ $out == *'"Example Domain"'* ]]; then
    echo "  open    a render in the pod, https://example.com/"
else
    echo "  SHUT    a render in the pod, https://example.com/: $(masked <<<"$out")"
    failed=1
fi

echo "  into the browser pod"
if out=$(kubectl -n "$NS" exec deploy/scout -- scout-browser probe render https://example.com/ 2>&1) && [[ $out == *'"Example Domain"'* ]]; then
    echo "  open    a render asked by Scout's pod, through the Service"
else
    echo "  SHUT    a render asked by Scout's pod: $(masked <<<"$out")"
    failed=1
fi
if out=$(in_control tcp scout-browser:8091); then
    echo "  OPEN    from a pod that is not Scout's: $(masked <<<"$out")"
    failed=1
else
    echo "  closed  from a pod that is not Scout's"
fi

echo "  in the browser pod"
named=$(kubectl -n "$NS" exec deploy/scout-browser -- env | cut -d= -f1 | grep -iE 'key|token|secret|password|passwd|credential' || true)
if [ -n "$named" ]; then
    echo "  HELD    variables named like a secret: $(tr '\n' ' ' <<<"$named")"
    failed=1
else
    echo "  none    no variable named like a key, token, secret or password"
fi
if kubectl -n "$NS" exec deploy/scout-browser -- test -e /var/run/secrets/kubernetes.io; then
    echo "  HELD    a service-account token"
    failed=1
else
    echo "  none    no service-account token"
fi
if kubectl -n "$NS" exec deploy/scout-browser -- touch /usr/local/bin/written 2>/dev/null; then
    echo "  HELD    a root it can write"
    failed=1
else
    echo "  none    a root it cannot write"
fi

exit "$failed"
ON_THE_NODE
