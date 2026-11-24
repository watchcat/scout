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

# These three configure the deploy rather than the bot, but they live in the
# same .env as everything else. Read from there when the environment does not
# already carry them — which is the normal case, because .env cannot be
# `source`d: its values are unquoted, so a line like
# `SCOUT_MAIL_FROM=Scout <scout@example.com>` is a redirect as far as bash is
# concerned. An exported value still wins, so a one-off deploy elsewhere is
# `SCOUT_SSH=root@other scripts/deploy-k3s.sh`.
env_file_value() {
    [ -f .env ] || return 0
    sed -n "s/^$1=//p" .env | head -1
}
for key in SCOUT_DOMAIN SCOUT_ACME_EMAIL SCOUT_SSH; do
    if [ -z "${!key:-}" ]; then
        export "$key=$(env_file_value "$key")"
    fi
done

: "${SCOUT_DOMAIN:?set SCOUT_DOMAIN — in .env or the environment}"
: "${SCOUT_ACME_EMAIL:?set SCOUT_ACME_EMAIL — in .env or the environment}"
: "${SCOUT_SSH:?set SCOUT_SSH to the node, e.g. root@203.0.113.4 — in .env or the environment}"

# Before anything is built: a malformed R2 key pair is not an error until restic
# runs at 03:30, and then only in a pod nobody reads. It shipped once and the
# off-site copy failed nine nights running.
. scripts/r2-keys.sh
if grep -qE '^AWS_' .env 2>/dev/null; then
    if ! why=$(r2_keys_problem "$(env_file_value AWS_ACCESS_KEY_ID)" "$(env_file_value AWS_SECRET_ACCESS_KEY)"); then
        echo "  refusing: $why" >&2
        echo "  (R2 → Manage API tokens shows the Access Key ID and Secret Access Key once, when the token is created)" >&2
        exit 1
    fi
fi

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
    # SCOUT_COMMIT goes into the binary, which says it on its metrics page:
    # the check at the end of this script.
    "${SSH[@]}" "cd $REMOTE_SRC && { setsid nohup docker build --build-arg SCOUT_COMMIT=$SHA -t scout:$SHA . > $BUILD_LOG 2>&1 < /dev/null & echo \$! > $BUILD_PID; } && echo '  pid' \$(cat $BUILD_PID)"
    STATE=building
fi

# Whether the image this deploy ships was built by it, or by an earlier run
# of it that this one found still building, rather than found built.
BUILT=0
if [ "$DRY_RUN" -eq 0 ] && [ "$STATE" = building ]; then
    BUILT=1
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
# .env is the single source of truth for secrets, but it is split in two on
# the way in, and the split is the point.
#
# The bot's Secret is mounted into its process with envFrom. That process
# fetches arbitrary web pages, runs headless Chromium on them and feeds the
# result to a language model — so it must not carry credentials that can
# delete every backup. Anything AWS_* or RESTIC_* goes to a separate Secret
# that only the backup CronJob reads.
#
# Both are piped in and never written to the node's disk.
#
# The deploy's own three go the same way for the same reason. They configure
# the ingress, the certificate issuer and this script; nothing in the bot
# reads them, and `envsubst` substitutes them into the manifests here rather
# than the container resolving them there. A process that renders untrusted
# pages should not be handed the address of the machine it runs on.
NOT_THE_BOTS='^(AWS_|RESTIC_)|^SCOUT_(DOMAIN|ACME_EMAIL|SSH)='
step "kubectl create secret scout (bot keys only)" \
    && grep -vE "$NOT_THE_BOTS" .env | "${SSH[@]}" 'kubectl -n scout create secret generic scout \
         --from-env-file=/dev/stdin --dry-run=client -o yaml | kubectl apply -f -'

# Only if there are any: the cluster is usable before R2 is set up.
if [ "$DRY_RUN" -eq 0 ] && grep -qE '^(AWS_|RESTIC_)' .env; then
    echo "  creating secret scout-offsite (backup keys only)"
    grep -E '^(AWS_|RESTIC_)' .env | "${SSH[@]}" 'kubectl -n scout create secret generic scout-offsite \
         --from-env-file=/dev/stdin --dry-run=client -o yaml | kubectl apply -f -'
elif [ "$DRY_RUN" -eq 1 ]; then
    echo "  (dry run) kubectl create secret scout-offsite (backup keys only, if present)"
fi

# The browser pod before Scout's, so that Scout's renders have somewhere to
# go once its new pod starts (deploy/k8s/browser.yaml). Its fence excepts
# the node's own addresses, one of them public, which are read from the node
# here and neither written down nor printed. envsubst is given the two names
# the file holds and fills in no others.
. scripts/node-except.sh
export SCOUT_SHA=$SHA
if step "read the node's own addresses, for the browser pod's fence"; then
    listing=$("${SSH[@]}" 'ip -4 -o addr show scope global')
    internal=$("${SSH[@]}" 'kubectl get nodes -o jsonpath="{.items[0].status.addresses[?(@.type==\"InternalIP\")].address}"')
    if ! SCOUT_NODE_EXCEPT=$(node_except "$listing" "$internal"); then
        echo "  refusing: $SCOUT_NODE_EXCEPT" >&2
        exit 1
    fi
    export SCOUT_NODE_EXCEPT
    echo "  $(tr ',' '\n' <<<"$SCOUT_NODE_EXCEPT" | wc -l | tr -d ' ') addresses, each excepted"
fi
if step "apply deploy/k8s/browser.yaml (the browser pod, its Service, its fence) and wait for it"; then
    running=$("${SSH[@]}" "kubectl -n scout get deployment/scout-browser -o jsonpath='{.spec.template.spec.containers[0].image}' 2>/dev/null" || true)
    envsubst '${SCOUT_SHA} ${SCOUT_NODE_EXCEPT}' < deploy/k8s/browser.yaml | "${SSH[@]}" 'kubectl apply -f -'
    # As for Scout's pod below: the same name built again is a new image.
    if [ "$running" = "scout:$SHA" ] && [ "$BUILT" -eq 1 ]; then
        echo "  the browser pod already ran scout:$SHA and this deploy built it again: restarting it onto it"
        "${SSH[@]}" "kubectl -n scout rollout restart deployment/scout-browser"
    fi
    if ! "${SSH[@]}" 'kubectl -n scout rollout status deployment/scout-browser --timeout=180s'; then
        echo >&2
        echo "  the browser pod's rollout did not complete" >&2
        "${SSH[@]}" 'kubectl -n scout get pods -l app=scout-browser; kubectl -n scout logs -l app=scout-browser --tail=40' >&2
        exit 1
    fi
    # Before Scout is pointed at it: a page rendered in the pod, asked of
    # itself, names resolved and Chromium started under its fence. Scout's
    # pod is still the old one here, rendering on its own.
    if rendered=$("${SSH[@]}" 'kubectl -n scout exec deploy/scout-browser -- env SCOUT_BROWSER_URL=http://127.0.0.1:8091 scout-browser probe render https://example.com/' 2>&1) \
        && [[ $rendered == *'"Example Domain"'* ]]; then
        echo "  it renders: $rendered"
    else
        echo >&2
        echo "  the browser pod does not render, and Scout is left as it was: $rendered" >&2
        exit 1
    fi
fi

for f in pvc service deployment ingress issuer; do
    step "apply deploy/k8s/$f.yaml" \
        && envsubst < "deploy/k8s/$f.yaml" | "${SSH[@]}" 'kubectl apply -f -'
done

# The backup job only exists once its credentials do. Applying it earlier
# would schedule something that fails every night with a missing secret,
# which is worse than not having it: a failing job nobody expects to work
# teaches everyone to ignore failing jobs.
if [ "$DRY_RUN" -eq 0 ] && grep -qE '^(AWS_|RESTIC_)' .env; then
    echo "  applying the off-site backup job"
    "${SSH[@]}" 'kubectl apply -f -' < deploy/k8s/backup-cronjob.yaml
elif [ "$DRY_RUN" -eq 1 ]; then
    echo "  (dry run) apply deploy/k8s/backup-cronjob.yaml, if its keys are set"
fi
# `set image` to the name the deployment already has changes nothing, and a
# Deployment replaces its pod only when its template changes. An image built
# again under the name it had (after the check at the end found it was not
# what its name said, and it was removed) is new under an old name, so the
# pod is restarted onto it.
if step "set image scout:$SHA (and restart the pod if it already ran that name)"; then
    running=$("${SSH[@]}" "kubectl -n scout get deployment/scout -o jsonpath='{.spec.template.spec.containers[0].image}'")
    "${SSH[@]}" "kubectl -n scout set image deployment/scout scout=scout:$SHA"
    if [ "$running" = "scout:$SHA" ] && [ "$BUILT" -eq 1 ]; then
        echo "  the deployment already ran scout:$SHA and this deploy built it again: restarting the pod onto it"
        "${SSH[@]}" "kubectl -n scout rollout restart deployment/scout"
    fi
fi

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

say "Asking the pod which commit it runs"
# The image's name says which commit it was built for, not what is in it:
# a build of a commit older than the node's last build used to compile
# nothing and leave that build's binary under the new name (the `touch` in
# the Dockerfile). The binary names the commit it was compiled from on its
# metrics page, which is asked from the node as scripts/metrics.sh asks
# it: the port is not the internet's. The rollout has waited for the pod
# to be ready, so only an ask that fails is tried again.
. scripts/shipped.sh
pod_metrics() {
    # Single-quoted: expanded on the node.
    "${SSH[@]}" 'ip=$(kubectl -n scout get pod -l app=scout --field-selector=status.phase=Running -o jsonpath="{.items[0].status.podIP}") && curl -fsS --max-time 10 "http://$ip:9090/metrics"'
}
if step "compare the pod's scout_build_info with $SHA"; then
    why=$(shipped_check "$SHA" pod_metrics) && shipped=0 || shipped=$?
    case "$shipped" in
        0) echo "  it runs $SHA" ;;
        2)
            echo >&2
            echo "  the rollout finished, but $why" >&2
            echo "  ask again with: scripts/metrics.sh scout_build_info" >&2
            exit 1 ;;
        *)
            echo >&2
            echo "  the deploy was named $SHA, but $why" >&2
            # Deploying again would find scout:$SHA built and ship it again.
            # Only docker's copy goes: the pod's is what it starts from if it
            # restarts before then, and the next import replaces it.
            if [ "$BUILT" -eq 0 ]; then
                # Found built: an earlier run of this deploy may have built it
                # again and been cut off before restarting the pod onto it.
                echo "  if an earlier deploy of $SHA was cut off after its build, the pod may only need restarting, on the node:" >&2
                echo "    kubectl -n scout rollout restart deployment/scout" >&2
                echo "  otherwise:" >&2
            fi
            echo "  scout:$SHA is not what its name says. Remove it from docker on the node, then deploy again:" >&2
            echo "    docker rmi scout:$SHA" >&2
            echo "  the deploy builds it afresh and restarts the pod onto it" >&2
            exit 1 ;;
    esac
fi

say "Proving the browser pod's fence"
# After both rollouts: the checks run the new image's probe in the browser
# pod, in Scout's, and in a control pod started from it.
if [ "$DRY_RUN" -eq 1 ]; then
    scripts/check-browser-fence.sh --dry-run
elif ! scripts/check-browser-fence.sh "$SHA"; then
    echo >&2
    echo "  the browser pod's fence is not what deploy/k8s/browser.yaml says it is: see above" >&2
    exit 1
fi

say "Deployed: $SHA $(git log --oneline -1 --format=%s)"
