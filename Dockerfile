# two-bot-next: single always-on Cloudflare Container (ADR 0001).
# Multi-stage: the builder needs the full Rust toolchain; the runtime image
# carries only the release binary on distroless/cc (glibc, libgcc, CA trust
# data; no shell, package manager or OpenSSL CLI). rustls uses platform/webpki
# roots, not OpenSSL. PID 1 receives the Container SIGTERM via the exec form.
#
# Multi-platform manifest digests keep tag names readable for Dependabot while
# making both stages immutable. The builder and distroless runtime are both
# Debian 13 (trixie), so the binary links against the same glibc it runs on.
FROM rust:1.94-trixie@sha256:652612f07bfbbdfa3af34761c1e435094c00dde4a98036132fca28c7bb2b165c AS builder

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
RUN cargo build --release --locked

# The runtime base already ships Debian trust data (ca-certificates) and the
# nonroot account (uid/gid 65532, home /home/nonroot); nothing is installed.
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2 AS runtime

# Non-root user: the bot never needs container root.
USER 65532:65532
WORKDIR /home/nonroot

COPY --from=builder --chown=65532:65532 /app/target/release/two-bot ./two-bot

# Liveness + readiness (also the DO keepalive targets, see wrangler/).
EXPOSE 8080
ENV LISTEN_ADDR=0.0.0.0:8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/home/nonroot/two-bot", "--healthcheck"]

ENTRYPOINT ["/home/nonroot/two-bot"]
