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
#   scripts/check-browser-fence.sh <tag> pod     the pod alone: its fence, what it holds, a render in it
#   scripts/check-browser-fence.sh <tag> scout   Scout's side: a render asked by Scout's pod, and Scout's
#                                                own word that it renders through the pod
#   scripts/check-browser-fence.sh <tag>         both
#   scripts/check-browser-fence.sh --dry-run     what would be asked, asking nothing
#
# <tag> is the image the control pod is started from, e.g. 486ec6a.
# scripts/deploy-k3s.sh runs `pod` once the browser pod is up and before
# Scout is pointed at it, and `scout` after Scout's rollout. It runs on the
# node over SSH, as the deploy does, and prints no address: the node's, the
# pods' and the gateway's are written <node>, <scout pod> and <gateway>.
set -euo pipefail
cd "$(dirname "$0")/.."

if [ "${1:-}" = "--dry-run" ]; then
    cat <<'PLAN'
  (dry run) pod: in the browser pod, each closed one with a control in an unfenced pod:
    closed: Scout's Service (scout:80), Scout's metrics (<scout pod>:9090),
            the Kubernetes API by its Service (kubernetes.default.svc:443) and on the node
            (<node>:6443), the node's SSH (<node>:22), the kubelet and SSH on the pod's
            gateway (<gateway>:10250, <gateway>:22), the cloud's metadata service
            (169.254.169.254:80), a datagram out (udp 1.1.1.1:53),
            the browser pod from a pod that is not Scout's
    open:   a name resolved (dns example.com), a page's server (tcp example.com:443),
            a render in the pod itself (render https://example.com/)
    held:   no key, token, secret or password in its environment, no volume but scratch,
            no service-account token, a root it cannot write
  (dry run) scout: a render asked by Scout's pod through the Service, and Scout's
            start-up line saying it renders with the browser pod
PLAN
    exit 0
fi

SHA=${1:?usage: scripts/check-browser-fence.sh <image tag> [pod|scout] | --dry-run}
PHASE=${2:-all}
if ! [[ $SHA =~ ^[0-9a-f]{7,40}$ ]]; then
    echo "  refusing: $SHA is not a commit's short name" >&2
    exit 1
fi
case "$PHASE" in pod | scout | all) ;; *) echo "  refusing: the phase is pod, scout or neither" >&2; exit 1 ;; esac
if [ -z "${SCOUT_SSH:-}" ] && [ -f .env ]; then
    SCOUT_SSH=$(sed -n 's/^SCOUT_SSH=//p' .env | head -1)
fi
: "${SCOUT_SSH:?set SCOUT_SSH to the node — in .env or the environment}"

ssh -o BatchMode=yes "$SCOUT_SSH" bash -s -- "$SHA" "$PHASE" <<'ON_THE_NODE'
set -uo pipefail
SHA=$1
PHASE=$2
NS=scout
CONTROL=scout-fence-control
INTERNAL=$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}')
# The node's IPv4 InternalIP; the cluster lists an IPv6 one beside it, and
# the pods have no IPv6 to reach that with.
NODE=$(tr ' ' '\n' <<<"$INTERNAL" | grep -E '^[0-9]+(\.[0-9]+){3}$' | head -1)
GATEWAY=$(ip -4 -o addr show cni0 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | head -1)
SCOUT_POD=$(kubectl -n "$NS" get pod -l app=scout --field-selector=status.phase=Running -o jsonpath='{.items[0].status.podIP}')
if [ -z "$NODE" ] || [ -z "$GATEWAY" ] || [ -z "$SCOUT_POD" ]; then
    echo "  the node's address, the pods' gateway or Scout's pod could not be read" >&2
    exit 1
fi
failed=0

# Every address that could be said, written by what it is.
masked() {
    local line
    line=$(cat)
    for address in $INTERNAL; do line=${line//"$address"/<node>}; done
    line=${line//"$SCOUT_POD"/<scout pod>}
    line=${line//"$GATEWAY"/<gateway>}
    printf '%s' "$line" | tr '\n' ' ' | cut -c1-240
}

in_browser() { kubectl -n "$NS" exec deploy/scout-browser -- scout-browser probe "$@" 2>&1; }
in_control() { kubectl -n "$NS" exec "$CONTROL" -- scout-browser probe "$@" 2>&1; }

# What the browser pod must not reach. Fenced means the probe itself ran
# and said FAIL, not that kubectl could not run it. `strict`: the control
# must reach it, or the check proves nothing and fails. `lenient`: an
# unreached control is said, and the check is not counted either way.
closed() {
    local how=$1 what=$2 out control
    shift 2
    out=$(in_browser "$@")
    case "$out" in
        OK*)
            echo "  OPEN     $what: the browser pod reached it: $(masked <<<"$out")"
            failed=1
            return ;;
        FAIL*) ;;
        *)
            echo "  UNASKED  $what: the probe did not run in the browser pod: $(masked <<<"$out")"
            failed=1
            return ;;
    esac
    control=$(in_control "$@")
    if [[ $control == OK* ]]; then
        echo "  closed   $what"
    elif [ "$how" = lenient ]; then
        echo "  closed   $what (unproved: the control could not reach it either)"
    else
        echo "  UNPROVED $what: the control could not reach it either: $(masked <<<"$control")"
        failed=1
    fi
}

# What the browser pod must reach.
open() {
    local what=$1 out
    shift
    out=$(in_browser "$@")
    if [[ $out == OK* ]]; then
        echo "  open     $what"
    else
        echo "  SHUT     $what: $(masked <<<"$out")"
        failed=1
    fi
}

# What the browser pod must not hold: `sh -c` in the pod prints one word,
# and only that word passes, so a failed exec is not taken for "none".
holds_nothing() {
    local what=$1 script=$2 none=$3 out
    out=$(kubectl -n "$NS" exec deploy/scout-browser -- sh -c "$script" 2>&1)
    if [ "$out" = "$none" ]; then
        echo "  none     $what"
    else
        echo "  HELD     $what: $(masked <<<"$out")"
        failed=1
    fi
}

if [ "$PHASE" != scout ]; then
    # The control: the same image, no label any policy selects, no token,
    # and gone on its own after a while if this script is cut off.
    kubectl -n "$NS" delete pod "$CONTROL" --ignore-not-found --wait=true >/dev/null 2>&1
    trap 'kubectl -n "$NS" delete pod "$CONTROL" --ignore-not-found --wait=false >/dev/null 2>&1' EXIT
    if ! kubectl -n "$NS" run "$CONTROL" --image="scout:$SHA" --image-pull-policy=IfNotPresent --restart=Never \
        --labels=role=fence-control --overrides='{"spec":{"automountServiceAccountToken":false,"activeDeadlineSeconds":900}}' \
        --command -- sleep 900 >/dev/null 2>&1 \
        || ! kubectl -n "$NS" wait --for=condition=Ready "pod/$CONTROL" --timeout=90s >/dev/null 2>&1; then
        echo "  the control pod did not start: $(kubectl -n "$NS" get pod "$CONTROL" -o jsonpath='{.status.phase} {.status.containerStatuses[0].state.waiting.reason}' 2>&1 | masked)" >&2
        exit 1
    fi

    echo "  out of the browser pod"
    closed strict  "Scout's Service, scout:80" tcp scout:80
    closed strict  "Scout's metrics, <scout pod>:9090" tcp "$SCOUT_POD:9090"
    closed strict  "the Kubernetes API by its Service, kubernetes.default.svc:443" tcp kubernetes.default.svc:443
    closed strict  "the Kubernetes API, <node>:6443" tcp "$NODE:6443"
    closed strict  "the node's SSH, <node>:22" tcp "$NODE:22"
    closed strict  "the kubelet on the pods' gateway, <gateway>:10250" tcp "$GATEWAY:10250"
    # Lenient: sshd may listen on the public address alone.
    closed lenient "SSH on the pods' gateway, <gateway>:22" tcp "$GATEWAY:22"
    closed lenient "the cloud's metadata service, 169.254.169.254:80" tcp 169.254.169.254:80
    closed strict  "a datagram out, a DNS question to 1.1.1.1:53" udp 1.1.1.1:53
    open "a name resolved, example.com" dns example.com
    open "a page's server, example.com:443" tcp example.com:443
    # The pod's own render, asked of itself on loopback: Chromium starts and
    # renders under this fence, this root and this user.
    out=$(kubectl -n "$NS" exec deploy/scout-browser -- env SCOUT_BROWSER_URL=http://127.0.0.1:8091 scout-browser probe render https://example.com/ 2>&1)
    if [[ $out == OK*'"Example Domain"'* ]]; then
        echo "  open     a render in the pod, https://example.com/"
    else
        echo "  SHUT     a render in the pod, https://example.com/: $(masked <<<"$out")"
        failed=1
    fi

    echo "  into the browser pod"
    out=$(in_control tcp scout-browser:8091)
    case "$out" in
        FAIL*) echo "  closed   from a pod that is not Scout's (Scout's own is asked in the scout phase)" ;;
        *) echo "  OPEN     from a pod that is not Scout's: $(masked <<<"$out")"; failed=1 ;;
    esac

    echo "  in the browser pod"
    # Names only; `-s` drops the lines of a value that runs over several.
    holds_nothing "variables named like a key, token, secret or password" \
        'env | cut -s -d= -f1 | grep -iE "key|token|secret|passw|credential" || echo none' none
    holds_nothing "a service-account token" 'test -e /var/run/secrets/kubernetes.io && echo held || echo none' none
    # /data is the image's, owned by the pod's own user: only a root that
    # cannot be written keeps it out.
    holds_nothing "a root it can write" \
        'if touch /data/written 2>/dev/null; then echo writable; else awk '\''$2 == "/" { if ($4 ~ /^ro(,|$)/) print "none"; else print "not read-only" }'\'' /proc/mounts; fi' none
    volumes=$(kubectl -n "$NS" get pod -l app=scout-browser -o jsonpath='{.items[*].spec.volumes}' 2>&1)
    if [ -n "$volumes" ] && ! grep -qE '"(secret|projected|hostPath|configMap|csi|persistentVolumeClaim|downwardAPI|serviceAccountToken)"' <<<"$volumes"; then
        echo "  none     a volume but scratch"
    else
        echo "  HELD     a volume that is not scratch: $(masked <<<"$volumes")"
        failed=1
    fi
fi

if [ "$PHASE" != pod ]; then
    echo "  Scout's side"
    out=$(kubectl -n "$NS" exec deploy/scout -- scout-browser probe render https://example.com/ 2>&1)
    if [[ $out == OK*'"Example Domain"'* ]]; then
        echo "  open     a render asked by Scout's pod, through the Service"
    else
        echo "  SHUT     a render asked by Scout's pod: $(masked <<<"$out")"
        failed=1
    fi
    # Scout's own word, from its start, on what it renders with: the probe
    # above reads SCOUT_BROWSER_URL itself, and this is what Scout made of it.
    # The log is written in colour: the codes come out before it is read.
    said=$(kubectl -n "$NS" logs deploy/scout 2>&1 | sed 's/\x1b\[[0-9;]*m//g' | grep -m1 'fallback renderer' || true)
    if [[ $said == *'renders_with="the browser pod, http://scout-browser:8091/render"'* ]]; then
        echo "  through  Scout renders with the browser pod"
    else
        echo "  LOCAL    Scout does not say it renders with the browser pod: $(masked <<<"${said:-no start-up line}")"
        failed=1
    fi
fi

exit "$failed"
ON_THE_NODE
