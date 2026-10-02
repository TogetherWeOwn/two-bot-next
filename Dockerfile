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
COPY crates/store/Cargo.toml crates/store/
COPY crates/testsupport/Cargo.toml crates/testsupport/
RUN mkdir -p src crates/core/src crates/discord/src crates/bot/src crates/cutover/src crates/store/src crates/testsupport/src \
    && echo '' > src/lib.rs \
    && echo 'fn main(){}' > crates/bot/src/main.rs \
    && echo '' > crates/core/src/lib.rs \
    && echo '' > crates/discord/src/lib.rs \
    && echo '' > crates/cutover/src/lib.rs \
    && echo '' > crates/store/src/lib.rs \
    && echo '' > crates/testsupport/src/lib.rs \
    && cargo fetch --locked

# Real sources; the release profile (opt-level=z, lto, strip) targets the
# `lite` 256 MiB ceiling from ADR 0001.
COPY . .
# Non-secret build provenance, compiled into readiness (not a runtime override).
ARG BOT_BUILD_REVISION=unknown
ARG BOT_BUILD_ID=unknown
RUN cargo build --release --locked

FROM debian:bookworm-slim AS certificates

ARG BOT_BUILD_REVISION=unknown
ARG BOT_BUILD_ID=unknown
LABEL org.opencontainers.image.revision=$BOT_BUILD_REVISION \
      com.togetherweown.build-id=$BOT_BUILD_ID

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

FROM debian:bookworm-slim AS runtime

# Keep the trust store, not the certificate installation tools/dependencies.
# Debian's hashed certificate links also need their shared-data targets.
COPY --from=certificates /etc/ssl/certs/ /etc/ssl/certs/
COPY --from=certificates /usr/share/ca-certificates/ /usr/share/ca-certificates/

# Verify the copied trust store before creating the non-root bot user.
RUN test -s /etc/ssl/certs/ca-certificates.crt \
    && test -z "$(find -L /etc/ssl/certs -type l -print)" \
    && useradd --create-home --shell /usr/sbin/nologin two-bot
USER two-bot
WORKDIR /home/two-bot

COPY --from=builder --chown=two-bot:two-bot /app/target/release/two-bot ./two-bot

# Liveness + readiness (also the DO keepalive targets, see wrangler/).
EXPOSE 8080
ENV LISTEN_ADDR=0.0.0.0:8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/home/two-bot/two-bot", "--healthcheck"]

ENTRYPOINT ["/home/two-bot/two-bot"]
