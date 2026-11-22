#!/bin/sh
# Runs the renderer's live tests against the Chromium the image ships.
#
# The renderer (`crates/scout-core/src/tools/browser.rs`) tells its Chrome to
# keep a page's WebRTC in the proxy, to load nothing ahead of a page and to
# ask less of its maker, all by settings and switches, and a Chromium that
# does not know one of them ignores it in silence. The tests that say they
# hold are ignored by `cargo test`: they want a real Chrome. On a laptop
# they run against the laptop's. This builds them into an image beside the
# Chromium that production runs (the `render-check` stage of the Dockerfile)
# and runs them there.
#
#   scripts/check-render-chromium.sh                  here, with docker
#   scripts/check-render-chromium.sh --on-node        on the node a deploy builds on
#   ... --walled https://shop.example/a-page          then one render of a page
#                                                     behind a challenge, printed
#
# `--on-node` is where the answer counts: a laptop builds the image for its
# own processor and installs whatever Chromium Debian has that day, while
# the node builds on the layer it last installed Chromium into, for as long
# as its build cache keeps that layer. Which Chromium that is, is printed,
# and the pod's own beside it: when they differ, what was tested is what
# the next deploy would ship and not what is running. It sends `git archive
# HEAD`, as a deploy does, to a directory of its own, and needs SCOUT_SSH
# from the environment or `.env`. The build runs over the connection: if
# that drops, run it again, and cargo picks up where it stopped. The
# directory and the image's name are removed however it ends; the node's
# build cache keeps the layers, the tests' binary among them, until it is
# pruned.
set -eu
cd "$(dirname "$0")/.."

on_node=0
walled=
while [ $# -gt 0 ]; do
    case "$1" in
        --on-node) on_node=1 ;;
        --walled) walled="${2:?--walled wants an address}"; shift ;;
        *) echo "usage: $0 [--on-node] [--walled <url>]" >&2; exit 2 ;;
    esac
    shift
done

if [ "$on_node" -eq 1 ]; then
    # The address is quoted for the shell there, and an address has no quote
    # in it. Said before anything is sent.
    case "$walled" in *\'*) echo "that address has a quote in it" >&2; exit 2 ;; esac
    if [ -z "${SCOUT_SSH:-}" ] && [ -f .env ]; then
        SCOUT_SSH=$(sed -n 's/^SCOUT_SSH=//p' .env | head -1)
    fi
    : "${SCOUT_SSH:?set SCOUT_SSH to the node — in .env or the environment}"
    there=/opt/scout-render-check
    # The build says nothing for minutes; the connection is kept up through them.
    on_the_node() { ssh -o BatchMode=yes -o ServerAliveInterval=30 "$SCOUT_SSH" "$@"; }
    tidy() { on_the_node "rm -rf $there; docker image rm scout-render-check >/dev/null 2>&1 || true"; }
    trap tidy EXIT
    trap 'exit 130' INT TERM
    git archive HEAD | on_the_node "rm -rf $there && mkdir -p $there && tar -x -C $there"
    pod=$(on_the_node "kubectl -n scout exec deployment/scout -- chromium --version 2>/dev/null" | tr -d "'" || true)
    on_the_node "cd $there && SCOUT_POD_CHROMIUM='$pod' sh scripts/check-render-chromium.sh ${walled:+--walled '$walled'}"
    exit 0
fi

image=scout-render-check
echo "==> Building the tests beside the image's Chromium"
docker build --quiet --target render-check -t "$image" . > /dev/null
chromium=$(docker run --rm "$image" chromium --version)
echo "$chromium, $(docker run --rm "$image" uname -m)"
if [ -n "${SCOUT_POD_CHROMIUM:-}" ]; then
    echo "the pod runs: $SCOUT_POD_CHROMIUM"
    [ "$chromium" = "$SCOUT_POD_CHROMIUM" ] || echo "NOT THE SAME: this is the Chromium the next deploy would ship, not the one running"
fi

# A link-local address on the container's own loopback, for the tests' pages
# to aim UDP at beside loopback and the container's private address. Giving
# it takes root and NET_ADMIN, inside the container and nowhere else; the
# tests then run as the image's user, as the bot does.
link_local=169.254.77.7
echo "==> The renderer's live tests"
docker run --rm --cap-add NET_ADMIN --user root "$image" sh -c "
    ip addr add $link_local/32 dev lo &&
    exec runuser -u scout -- env HOME=/home/scout SCOUT_PROBE_UDP_ADDRS=$link_local \
        scout-core-tests --ignored --nocapture --test-threads=1 tools::browser::live:: --skip renders_a_walled_page"

if [ -n "$walled" ]; then
    echo "==> One page behind a challenge"
    docker run --rm -e SCOUT_PROBE_URL="$walled" "$image" \
        scout-core-tests --ignored --nocapture tools::browser::live::renders_a_walled_page
fi
