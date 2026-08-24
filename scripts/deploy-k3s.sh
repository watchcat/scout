#!/usr/bin/env bash
# Deploy scout to the k3s node.
#
# Runs HERE and builds THERE. The build has to happen on the node because this
# repository is developed on arm64 and the server is x86_64 — buildx can target
# amd64 locally, but only by emulating the whole DuckDB C++ compile, which
# takes hours rather than minutes.
#
# The source crosses as `git archive HEAD`, so what is built is exactly the
# commit this deploy claims to be: no clone, no credentials on the node, and
# nothing untracked or gitignored can sneak into the image. `.env` is not in
# that archive and never lands on the node's disk — it is piped into `kubectl
# create secret` and forgotten.
#
#   scripts/deploy-k3s.sh            build, apply, wait
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
REMOTE_SRC=/opt/scout

# A deploy is named by what is in git, so a rollback is a tag that still exists
# in containerd rather than a rebuild that might not reproduce.
SHA=$(git rev-parse --short HEAD)
if [ -n "$(git status --porcelain -- crates Cargo.toml Cargo.lock Dockerfile deploy scripts)" ]; then
    echo "  refusing: uncommitted changes would ship as $SHA without being $SHA" >&2
    git status --short -- crates Cargo.toml Cargo.lock Dockerfile deploy scripts >&2
    exit 1
fi

say "Sending $SHA to $SCOUT_SSH"
step "git archive HEAD | ssh | tar -x -C $REMOTE_SRC" \
    && git archive HEAD | "${SSH[@]}" "rm -rf $REMOTE_SRC && mkdir -p $REMOTE_SRC && tar -x -C $REMOTE_SRC"

say "Building scout:$SHA there"
# The first build compiles DuckDB's C++ and takes about ten minutes; later ones
# reuse the cache mounts and take about twenty seconds.
step "docker build -t scout:$SHA" \
    && "${SSH[@]}" "cd $REMOTE_SRC && docker build -t scout:$SHA ."
step "docker save | k3s ctr images import" \
    && "${SSH[@]}" "docker save scout:$SHA | k3s ctr images import -"

say "Applying"
step "kubectl apply namespace" \
    && "${SSH[@]}" 'kubectl apply -f -' < deploy/k8s/namespace.yaml
# .env is piped in, used, and gone. The values do end up in the cluster's
# datastore, which is the same trust boundary they already have here.
step "kubectl create secret from .env (piped, never written)" \
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
