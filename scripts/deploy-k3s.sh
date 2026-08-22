#!/usr/bin/env bash
# Deploy scout to the k3s node.
#
# Runs HERE, not on the node. The image is built locally and shipped over SSH
# into k3s's containerd; the manifests are piped through SSH to kubectl. The
# server therefore needs no Docker, no build toolchain, and no copy of this
# repository — it runs k3s and Scout and nothing else, which is both cheaper
# and less to attack.
#
# The other half of that trade: a deploy needs a machine with Docker, and the
# first image push is a slow upload.
#
#   scripts/deploy-k3s.sh            build, ship, apply, wait
#   scripts/deploy-k3s.sh --dry-run  print the plan, change nothing
set -euo pipefail
cd "$(dirname "$0")/.."

DRY_RUN=0
[ "${1:-}" = "--dry-run" ] && DRY_RUN=1

say() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }
step() { if [ "$DRY_RUN" -eq 1 ]; then echo "  (dry run) $1"; return 1; fi; return 0; }

: "${SCOUT_DOMAIN:?set SCOUT_DOMAIN — it is in .env}"
: "${SCOUT_ACME_EMAIL:?set SCOUT_ACME_EMAIL — it is in .env}"
: "${SCOUT_SSH:?set SCOUT_SSH to the node, e.g. root@203.0.113.4}"

SSH=(ssh -o BatchMode=yes "$SCOUT_SSH")

# A deploy is named by what is in git, so a rollback is a tag that still exists
# in containerd rather than a rebuild that might not reproduce.
SHA=$(git rev-parse --short HEAD)
if [ -n "$(git status --porcelain -- crates Cargo.toml Cargo.lock Dockerfile deploy scripts)" ]; then
    echo "  refusing: uncommitted changes would ship as $SHA without being $SHA" >&2
    git status --short -- crates Cargo.toml Cargo.lock Dockerfile deploy scripts >&2
    exit 1
fi

say "Building scout:$SHA here"
step "docker build -t scout:$SHA ." && docker build -t "scout:$SHA" .

say "Shipping it to $SCOUT_SSH"
# gzip -1 rather than the default: the layers that matter are already
# compressed, so the cheap setting gets most of the saving for a fraction of
# the CPU. Expect minutes on the first push and seconds afterwards, since only
# the top layer changes.
step "docker save | gzip | ssh | k3s ctr images import" \
    && docker save "scout:$SHA" | gzip -1 | "${SSH[@]}" 'gunzip | k3s ctr images import -'

say "Applying"
step "kubectl apply namespace" \
    && "${SSH[@]}" 'kubectl apply -f -' < deploy/k8s/namespace.yaml
# .env never lands on the node: it is piped in, used, and gone. The values do
# end up in the cluster's datastore, which is the same trust boundary they
# already have on this machine.
step "kubectl create secret from .env (piped, not copied)" \
    && "${SSH[@]}" 'kubectl -n scout create secret generic scout \
         --from-env-file=/dev/stdin --dry-run=client -o yaml | kubectl apply -f -' < .env
for f in pvc service deployment ingress issuer; do
    step "apply deploy/k8s/$f.yaml" \
        && envsubst < "deploy/k8s/$f.yaml" | "${SSH[@]}" 'kubectl apply -f -'
done
step "set image scout:$SHA" \
    && "${SSH[@]}" "kubectl -n scout set image deployment/scout scout=scout:$SHA"

say "Waiting for the rollout"
# Recreate stops the old pod before starting the new one, so this window
# includes the drain: up to 330s when a request was in flight.
if step "kubectl rollout status"; then
    if ! "${SSH[@]}" 'kubectl -n scout rollout status deployment/scout --timeout=420s'; then
        echo >&2
        echo "  the rollout did not complete" >&2
        "${SSH[@]}" 'kubectl -n scout get pods; kubectl -n scout logs -l app=scout --tail=40' >&2
        exit 1
    fi
fi

say "Deployed: $SHA $(git log --oneline -1 --format=%s)"
