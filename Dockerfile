# Build stage: dependency layer is cached separately so code changes don't
# recompile DuckDB (the expensive part).
FROM rust:bookworm AS builder
# DuckDB's C++ compile is memory-hungry; full parallelism OOMs the Docker VM
# (observed: 10 jobs vs ~8 GiB VM RAM hangs the build). 4 jobs fits.
ENV CARGO_BUILD_JOBS=4
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release

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
COPY --from=builder /app/target/release/scout /usr/local/bin/scout
USER scout
WORKDIR /data
ENV SCOUT_DB_PATH=/data/scout.duckdb
ENV SCOUT_CHROME=/usr/bin/chromium
CMD ["scout"]
