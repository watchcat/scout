#!/usr/bin/env bash
#
# Deploy scout without cutting anyone off mid-answer.
#
# `docker compose up -d --build` builds and replaces in one step, so the old
# container is killed while it may still be working on a request — and a
# request can legitimately run for RUN_BUDGET (300s). This does it in two
# steps instead: build first while the old one keeps serving, then hand over.
#
# The drain itself is not this script's doing. The bot handles SIGTERM by
# refusing new updates and awaiting the handlers already running (see
# bot::run), and compose.yaml gives it 330s to finish. This script only
# sequences things so that window is short and visible.
#
# Usage:
#   scripts/deploy.sh              build, test, drain, deploy
#   scripts/deploy.sh --no-tests   skip cargo test (CI already ran it)
#   scripts/deploy.sh --dry-run    show what would happen, change nothing

set -euo pipefail
cd "$(dirname "$0")/.."

RUN_TESTS=1
DRY_RUN=0
for arg in "$@"; do
    case "$arg" in
        --no-tests) RUN_TESTS=0 ;;
        --dry-run)  DRY_RUN=1 ;;
        -h|--help)  sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $arg (try --help)" >&2; exit 2 ;;
    esac
done

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

# A container that is already down needs no draining, and a rebuild of a
# never-started service is just a start.
running=$(docker compose ps --status running --quiet 2>/dev/null | wc -l | tr -d ' ')

say "Checking the working tree"
if [ -n "$(git status --porcelain -- src Cargo.toml Cargo.lock compose.yaml Dockerfile)" ]; then
    echo "  uncommitted changes — deploying code that is not in git:"
    git status --short -- src Cargo.toml Cargo.lock compose.yaml Dockerfile | sed 's/^/    /'
else
    echo "  clean: $(git log --oneline -1)"
fi

if [ "$RUN_TESTS" -eq 1 ]; then
    say "Running tests"
    if [ "$DRY_RUN" -eq 1 ]; then echo "  (dry run) cargo test"; else cargo test --quiet; fi
fi

# Built before anything is stopped, so a compile error costs no downtime at
# all and the handover below is seconds rather than minutes.
say "Building the new image (current version still serving)"
if [ "$DRY_RUN" -eq 1 ]; then echo "  (dry run) docker compose build"; else docker compose build; fi

if [ "$running" -eq 0 ]; then
    say "Nothing is running — starting fresh"
else
    say "Handing over"
    echo "  The running container gets SIGTERM, stops accepting new messages,"
    echo "  and finishes whatever it is already doing (up to 330s)."
    echo "  Watch it drain:  docker compose logs -f"
fi

if [ "$DRY_RUN" -eq 1 ]; then
    echo "  (dry run) docker compose up -d"
    exit 0
fi

started_at=$(date +%s)
docker compose up -d
echo "  handover took $(( $(date +%s) - started_at ))s"

say "Waiting for the new version to come up"
for _ in $(seq 1 60); do
    if docker compose logs --since 5m 2>/dev/null | grep -q "scout is up"; then
        docker compose logs --since 5m 2>&1 |
            grep -E "in-flight requests|flight search|scout is up" |
            sed -E 's/\x1b\[[0-9;]*m//g; s/^scout-1 *\| *//' |
            tail -5 |
            sed 's/^/  /'
        say "Deployed: $(git log --oneline -1)"
        exit 0
    fi
    sleep 2
done

echo "  the new container did not report 'scout is up' within 120s" >&2
docker compose ps
docker compose logs --tail 30
exit 1
