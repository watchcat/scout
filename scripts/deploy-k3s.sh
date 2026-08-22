#!/usr/bin/env bash
# Deploy scout to the k3s node.
#
# Runs ON the node: builds with docker, imports into k3s's own containerd, and
# applies. There is no registry, which is the right trade for one machine — and
# it means the image only ever exists where it is used.
#
#   scripts/deploy-k3s.sh            build, import, apply, wait
#   scripts/deploy-k3s.sh --dry-run  print the plan, change nothing
set -euo pipefail
cd "$(dirname "$0")/.."

DRY_RUN=0
[ "${1:-}" = "--dry-run" ] && DRY_RUN=1

say() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }
run() { if [ "$DRY_RUN" -eq 1 ]; then echo "  (dry run) $*"; else "$@"; fi; }

: "${SCOUT_DOMAIN:?set SCOUT_DOMAIN — it is in .env}"
: "${SCOUT_ACME_EMAIL:?set SCOUT_ACME_EMAIL — it is in .env}"

# A deploy is named by what is in git, so a rollback is a tag that still exists
# in containerd rather than a rebuild that might not reproduce.
SHA=$(git rev-parse --short HEAD)
if [ -n "$(git status --porcelain -- crates Cargo.toml Cargo.lock Dockerfile deploy scripts)" ]; then
    echo "  refusing: uncommitted changes would ship as $SHA without being $SHA" >&2
    git status --short -- crates Cargo.toml Cargo.lock Dockerfile deploy scripts >&2
    exit 1
fi

say "Building scout:$SHA"
run docker build -t "scout:$SHA" .
if [ "$DRY_RUN" -eq 1 ]; then echo "  (dry run) docker save | k3s ctr images import -"; else
    docker save "scout:$SHA" | k3s ctr images import -
fi

say "Applying"
run kubectl apply -f deploy/k8s/namespace.yaml
if [ "$DRY_RUN" -eq 1 ]; then echo "  (dry run) kubectl create secret generic scout --from-env-file=.env"; else
    # Rebuilt every deploy, so rotating a key is editing .env and deploying.
    # create --dry-run | apply is the documented way to make it idempotent.
    kubectl -n scout create secret generic scout \
        --from-env-file=.env --dry-run=client -o yaml | kubectl apply -f -
fi
for f in pvc service deployment ingress issuer; do
    if [ "$DRY_RUN" -eq 1 ]; then echo "  (dry run) apply deploy/k8s/$f.yaml"; else
        envsubst < "deploy/k8s/$f.yaml" | kubectl apply -f -
    fi
done
run kubectl -n scout set image deployment/scout "scout=scout:$SHA"

say "Waiting for the rollout"
if [ "$DRY_RUN" -eq 1 ]; then echo "  (dry run) kubectl rollout status"; else
    # Recreate stops the old pod before starting the new one, so this window
    # includes the drain: up to 330s when a request was in flight.
    if ! kubectl -n scout rollout status deployment/scout --timeout=420s; then
        echo >&2
        echo "  the rollout did not complete" >&2
        kubectl -n scout get pods >&2
        kubectl -n scout logs -l app=scout --tail=40 >&2
        exit 1
    fi
fi

say "Deployed: $SHA $(git log --oneline -1 --format=%s)"
