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

# The build runs detached on the node and this script watches it. That is not
# tidiness: a synchronous `ssh ... docker build` dies with the connection, and
# ten minutes of DuckDB compilation is long enough for a laptop to sleep or a
# network to blink. Detaching also makes this script resumable — re-run it after
# a dropped connection and it attaches to the build already in flight rather
# than starting a second one on top of it.
BUILD_LOG="/var/log/scout-build-$SHA.log"
BUILD_PID="/run/scout-build-$SHA.pid"

# Liveness comes from a pidfile, not from pgrep. `pgrep -f "docker build -t
# scout:$SHA"` matches the ssh wrapper running the check, because that
# wrapper's own command line contains the pattern — so it reports "building"
# forever, the script never starts a build, and it watches one that does not
# exist. That is not hypothetical; it is what the first version did.
remote_state() {
    "${SSH[@]}" "
        if docker image inspect scout:$SHA >/dev/null 2>&1; then echo built
        elif [ -f $BUILD_PID ] && kill -0 \$(cat $BUILD_PID) 2>/dev/null; then echo building
        else echo absent; fi"
}

say "Checking $SCOUT_SSH for scout:$SHA"
if [ "$DRY_RUN" -eq 1 ]; then
    echo "  (dry run) would check, send, build, and watch"
    STATE=absent
else
    STATE=$(remote_state)
    echo "  $STATE"
fi

if [ "$DRY_RUN" -eq 0 ] && [ "$STATE" = absent ]; then
    say "Sending $SHA"
    # git archive, so what is built is exactly this commit: nothing untracked,
    # nothing gitignored, and no .env.
    git archive HEAD | "${SSH[@]}" "rm -rf $REMOTE_SRC && mkdir -p $REMOTE_SRC && tar -x -C $REMOTE_SRC"

    say "Starting the build (detached; first one compiles DuckDB, ~10 min)"
    "${SSH[@]}" "cd $REMOTE_SRC && { setsid nohup docker build -t scout:$SHA . > $BUILD_LOG 2>&1 < /dev/null & echo \$! > $BUILD_PID; } && echo '  pid' \$(cat $BUILD_PID)"
    STATE=building
fi

if [ "$DRY_RUN" -eq 0 ] && [ "$STATE" = building ]; then
    say "Watching the build"
    while :; do
        case "$(remote_state)" in
            built) echo "  done"; break ;;
            building)
                "${SSH[@]}" "tail -n 20 $BUILD_LOG 2>/dev/null | grep -vE '^\\s*$' | tail -1 | cut -c1-100" \
                    | sed 's/^/  /'
                sleep 15 ;;
            absent)
                echo >&2
                echo "  the build stopped without producing an image" >&2
                "${SSH[@]}" "tail -n 30 $BUILD_LOG 2>/dev/null" >&2
                exit 1 ;;
            *)
                echo "  could not read the node's state" >&2
                exit 1 ;;
        esac
    done
fi

say "Importing into containerd"
# k3s does not see docker's images; they live in different stores.
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
