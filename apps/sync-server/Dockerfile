FROM rust:1.88-bookworm AS builder

WORKDIR /src
COPY . .
RUN cargo build --locked --release -p sync-server

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /nonexistent --shell /usr/sbin/nologin sync-server \
    && mkdir -p /data \
    && chown sync-server:sync-server /data

COPY --from=builder /src/target/release/sync-server /usr/local/bin/sync-server

USER sync-server

ENV SYNC_SERVER_BIND=0.0.0.0:8787 \
    SYNC_SERVER_DATA_DIR=/data \
    RUST_LOG=sync_server=info,tower_http=info

EXPOSE 8787
VOLUME ["/data"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl --fail --silent --show-error http://127.0.0.1:8787/health || exit 1

ENTRYPOINT ["/usr/local/bin/sync-server"]
