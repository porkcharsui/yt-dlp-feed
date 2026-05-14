# syntax=docker/dockerfile:1

FROM rust:1-slim-bookworm AS builder

WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        git \
        libssl-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        ffmpeg \
        libssl3 \
        yt-dlp \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home --home-dir /var/lib/yt-dlp-feed --shell /usr/sbin/nologin yt-dlp-feed \
    && mkdir -p /config /data \
    && chown -R yt-dlp-feed:yt-dlp-feed /var/lib/yt-dlp-feed /data

COPY --from=builder /app/target/release/yt-dlp-feed /usr/local/bin/yt-dlp-feed

USER yt-dlp-feed
WORKDIR /var/lib/yt-dlp-feed

ENV YT_DLP_FEED_CONFIG=/config/config.yaml

EXPOSE 8080
VOLUME ["/data"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1

ENTRYPOINT ["yt-dlp-feed"]
