# Build stage: dependency layer is cached separately so code changes don't
# recompile DuckDB (the expensive part).
FROM rust:bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release

# Runtime stage: slim image with just TLS deps.
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m scout \
    && mkdir -p /data \
    && chown scout:scout /data
COPY --from=builder /app/target/release/scout /usr/local/bin/scout
USER scout
WORKDIR /data
ENV SCOUT_DB_PATH=/data/scout.duckdb
CMD ["scout"]
