# syntax=docker/dockerfile:1

# Build stage. Cargo's registry and target directory live in BuildKit caches
# instead of image layers, which is what keeps rebuilds cheap:
#   * a build compiles the workspace's own crates, all of them, and no
#     dependency unless Cargo.lock changed it (about 3 minutes on the node)
#   * a Cargo.toml change recompiles the changed dependency alone, instead of
#     DuckDB's C++ from scratch — that is the ~10 minute build, and touching
#     dependencies three times in an afternoon paid it three times.
# The layer-based dependency trick this replaces could only do the first.
FROM rust:bookworm AS builder
# DuckDB's C++ compile is memory-hungry; full parallelism OOMs the Docker VM
# (observed: 10 jobs vs ~8 GiB VM RAM hangs the build). 4 jobs fits.
ENV CARGO_BUILD_JOBS=4
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# The binaries have to be copied out within this step: a cache mount is not
# part of the layer that results from it.
# The two packages the image ships, and what they depend on: the bot, and
# `scout-browser`, which the browser pod runs from this same image
# (`deploy/k8s/browser.yaml`). It is a second binary with all of scout-core
# in it, linked on every deploy, so that the pod renders with the very code
# Scout's own pod rendered with.
#
# The commit the build is of, which the binary says on its metrics page
# (`scout_build_info`, from `scout_api::commit`). `scripts/deploy-k3s.sh`
# names it, and a deploy compares it with what it built. Its value is part
# of this step's cache key, so every deploy compiles, a commit that leaves
# the crates alone too.
ARG SCOUT_COMMIT
# The `touch` is for cargo, which calls a crate built when none of its
# sources is newer than its last build, whatever is in them, and the target
# directory above is the last build's. Sent by `git archive`, every file
# carries its commit's time, so a commit older than the moment the last
# build here began compiling scout-api compiled nothing, and its image held
# that build's binary: measured on the node on 2026-11-23, with a commit
# dated a minute before the build it then followed began. A `COPY` the
# build cache already holds keeps the times of the build that made it, so
# the times are set here, not as the files arrive; and in this step, under
# the target directory's lock, so they cannot be older than a build that
# held the lock first.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/app/target,sharing=locked \
    find crates -type f -exec touch {} + \
    && cargo build --release -p scout-telegram -p scout-browser \
    && cp target/release/scout-telegram /scout-telegram \
    && cp target/release/scout-browser /scout-browser

# Runtime stage: CA certificates (TLS itself is rustls, inside the binary)
# plus Chromium, used to re-open pages that refuse a plain HTTP client (a
# shop behind a challenge answers 403 to reqwest and serves the product
# page to a real browser). In production those renders run in the browser
# pod, from this same image; Scout's own pod keeps Chromium for the trip
# PDF, which prints Scout's own page, and a local run renders with it.
# Chromium is the bulk of this image; drop it and the bot still runs,
# minus that fallback.
FROM debian:bookworm-slim AS runtime-base
# No `chmod 4755` on a setuid sandbox here: Debian's chromium ships none —
# there is no `chrome-sandbox` binary in /usr/lib/chromium at all, so the
# chmod fails the build outright. It isolates renderers with user
# namespaces instead, which this container has. Measured in the running
# pod as uid 1000: `chromium --headless=new --print-to-pdf` writes a real
# PDF with no sandbox binary and no --no-sandbox flag.
# poppler-utils for `pdftoppm`: a PDF printed as drawn shapes ("Microsoft:
# Print To PDF" does this) has no text to read, so the inbox worker draws
# its pages and has the model read the pictures.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates chromium fonts-liberation tini poppler-utils \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m scout \
    && mkdir -p /data \
    && chown scout:scout /data
ENV SCOUT_CHROME=/usr/bin/chromium

# The renderer's live tests, compiled from the same sources as the binary, to
# be run against the Chromium installed above: `scripts/check-render-chromium.sh`.
# What keeps a page's WebRTC in the render's proxy, and the rest of what the
# renderer tells a Chrome, is settings and switches, and a Chromium that does
# not know one of them ignores it in silence; they were first measured on a
# laptop's Chrome, which is not this one. Neither stage is part of the image:
# a build with no `--target` makes the last stage of this file and what that
# stage is built from, which these two are not.
FROM builder AS render-tests
# One of the crate's tests reads the README, which the build above has no
# use for.
COPY README.md ./
# `--release`, to use what the build above has already compiled: DuckDB is
# not built a second time, only what the tests' own dependencies change.
#
# The `touch` is for cargo, which calls a crate built when none of its
# sources is newer than its last build, whatever is in them. Sent by `git
# archive`, as the check sends them to the node, every file carries its
# commit's time: the tests of a commit made before the last build here
# would not be compiled, and that build's tests would run under its name.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/app/target,sharing=locked \
    find crates README.md -type f -exec touch {} + \
    && cargo test --release --no-run --lib -p scout-core --message-format=json-render-diagnostics > /tmp/tests.json \
    && cp "$(sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' /tmp/tests.json | tail -1)" /scout-core-tests

FROM runtime-base AS render-check
# `ip`, with which the script gives the container a link-local address for a
# test's page to aim at.
RUN apt-get update \
    && apt-get install -y --no-install-recommends iproute2 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=render-tests /scout-core-tests /usr/local/bin/scout-core-tests
USER scout
WORKDIR /tmp
ENTRYPOINT ["/usr/bin/tini", "--"]
# Every live test of the renderer but the one that wants a shop's address.
CMD ["scout-core-tests", "--ignored", "--nocapture", "--test-threads=1", "tools::browser::live::", "--skip", "renders_a_walled_page"]

# The image itself. Last, so that it is what a plain `docker build` makes.
FROM runtime-base
COPY --from=builder /scout-telegram /usr/local/bin/scout-telegram
COPY --from=builder /scout-browser /usr/local/bin/scout-browser
USER scout
WORKDIR /data
ENV SCOUT_DB_PATH=/data/scout.duckdb
# Chromium's helper processes (renderer, zygote, crashpad) outlive the process
# that spawned them and are reparented to PID 1. Without an init there they are
# never reaped: one render left 35 zombies, which ends in an exhausted PID
# table. Compose can supply one with `init: true`; Kubernetes has no equivalent,
# so it belongs in the image, where every runtime gets it for free.
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["scout-telegram"]
