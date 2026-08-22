# syntax=docker/dockerfile:1

# Build stage. Cargo's registry and target directory live in BuildKit caches
# instead of image layers, which is what keeps rebuilds cheap:
#   * a source change recompiles the touched crate(s) alone (~20s)
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
# The binary has to be copied out within this step: a cache mount is not part
# of the layer that results from it.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/app/target,sharing=locked \
    cargo build --release \
    && cp target/release/scout-telegram /scout-telegram

# Runtime stage: TLS deps plus Chromium, used only to re-open pages that
# refuse a plain HTTP client (a shop behind a challenge answers 403 to
# reqwest and serves the product page to a real browser). Chromium is the
# bulk of this image; drop it and the bot still runs, minus that fallback.
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates libssl3 chromium fonts-liberation \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m scout \
    && mkdir -p /data \
    && chown scout:scout /data
COPY --from=builder /scout-telegram /usr/local/bin/scout-telegram
USER scout
WORKDIR /data
ENV SCOUT_DB_PATH=/data/scout.duckdb
ENV SCOUT_CHROME=/usr/bin/chromium
CMD ["scout-telegram"]
