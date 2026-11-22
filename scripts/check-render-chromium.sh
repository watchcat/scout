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
# the node keeps the layer it installed Chromium into for as long as the
# lines above it in the Dockerfile do not change. It sends `git archive
# HEAD`, as a deploy does, to a directory of its own, and needs SCOUT_SSH
# from the environment or `.env`. The build runs over the connection: if
# that drops, run it again, and cargo picks up where it stopped.
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
    if [ -z "${SCOUT_SSH:-}" ] && [ -f .env ]; then
        SCOUT_SSH=$(sed -n 's/^SCOUT_SSH=//p' .env | head -1)
    fi
    : "${SCOUT_SSH:?set SCOUT_SSH to the node — in .env or the environment}"
    there=/opt/scout-render-check
    git archive HEAD | ssh -o BatchMode=yes "$SCOUT_SSH" "rm -rf $there && mkdir -p $there && tar -x -C $there"
    # The address is quoted for the shell there; an address has no quote in it.
    case "$walled" in *\'*) echo "that address has a quote in it" >&2; exit 2 ;; esac
    status=0
    ssh -o BatchMode=yes "$SCOUT_SSH" "cd $there && sh scripts/check-render-chromium.sh ${walled:+--walled '$walled'}" || status=$?
    ssh -o BatchMode=yes "$SCOUT_SSH" "rm -rf $there; docker image rm scout-render-check >/dev/null 2>&1 || true"
    exit "$status"
fi

image=scout-render-check
echo "==> Building the tests beside the image's Chromium"
docker build --quiet --target render-check -t "$image" . > /dev/null
docker run --rm "$image" sh -c 'chromium --version; uname -m'

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
