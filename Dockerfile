# two-bot-next: single always-on Cloudflare Container (ADR 0001).
# Multi-stage: the builder needs the full Rust toolchain; the runtime image
# carries only the static-ish release binary + CA certs (rustls uses
# platform/webpki roots, no OpenSSL) and tini-style signal handling via
# the exec form below (PID 1 receives the Container SIGTERM).
#
# Build args (set via wrangler [[containers]] image_vars or docker build):
#   RUST_VERSION  pinned toolchain (default: stable matching rust-toolchain.toml)

ARG RUST_VERSION=1.94-bookworm
FROM rust:${RUST_VERSION} AS builder

WORKDIR /app

# Dependency layer first: manifests alone maximise cache hits.
COPY Cargo.toml Cargo.lock ./
COPY crates/core/Cargo.toml crates/core/
COPY crates/discord/Cargo.toml crates/discord/
COPY crates/bot/Cargo.toml crates/bot/
COPY crates/cutover/Cargo.toml crates/cutover/
RUN mkdir -p crates/core/src crates/discord/src crates/bot/src crates/cutover/src \
    && echo 'fn main(){}' > crates/bot/src/main.rs \
    && echo '' > crates/core/src/lib.rs \
    && echo '' > crates/discord/src/lib.rs \
    && echo '' > crates/cutover/src/lib.rs \
    && cargo fetch --locked

# Real sources; the release profile (opt-level=z, lto, strip) targets the
# `lite` 256 MiB ceiling from ADR 0001.
COPY . .
RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root user: the bot never needs container root.
RUN useradd --create-home --shell /usr/sbin/nologin two-bot
USER two-bot
WORKDIR /home/two-bot

COPY --from=builder --chown=two-bot:two-bot /app/target/release/two-bot ./two-bot

# Liveness + readiness (also the DO keepalive targets, see wrangler/).
EXPOSE 8080
ENV LISTEN_ADDR=0.0.0.0:8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/home/two-bot/two-bot", "--healthcheck"]

ENTRYPOINT ["/home/two-bot/two-bot"]
