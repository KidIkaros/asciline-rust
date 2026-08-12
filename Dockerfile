# asciline-server — multi-stage build
# Builder: pinned Rust (1.87 = the crate's MSRV, is_multiple_of) + cached deps
FROM rust:1.87-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY web ./web
RUN cargo build --release --bin asciline-server

# Runtime: binary + ffmpeg/ffprobe, non-root user, health check
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --shell /usr/sbin/nologin asciline

WORKDIR /srv/asciline
COPY --from=builder /build/target/release/asciline-server /usr/local/bin/asciline-server
COPY --from=builder /build/web ./web

# Pre-create the media volume owned by the non-root user: a bind mount owned
# by a host user would otherwise be unreadable by `asciline`.
RUN mkdir -p /srv/asciline/videos && chown asciline:asciline /srv/asciline/videos

USER asciline
EXPOSE 8000

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8000/healthz || exit 1

# Mount your media library here, e.g. -v /path/to/videos:/srv/asciline/videos
VOLUME ["/srv/asciline/videos"]

ENTRYPOINT ["asciline-server", "--host", "0.0.0.0"]
