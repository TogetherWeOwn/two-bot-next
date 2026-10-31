# voice-probe: short-lived UDP experiment image for the music voice-node
# spike. Same builder/runtime pattern as the repo-root Dockerfile: full
# toolchain builder, distroless/cc nonroot runtime, digest-pinned Debian 13
# (trixie) bases so the binary links against the glibc it runs on.
#
# Build from the repo root (the probe is a workspace member):
#   docker build -f crates/voice-probe/Dockerfile -t voice-probe .
FROM rust:1.98-trixie@sha256:a8a5f0a1e5fe7dfe1d352591e4a1c7dd2c08fd70475cae872cf3458ba0df0546 AS builder

WORKDIR /app

# Dependency layer first: manifests alone maximise cache hits.
COPY Cargo.toml Cargo.lock ./
COPY crates/core/Cargo.toml crates/core/
COPY crates/discord/Cargo.toml crates/discord/
COPY crates/bot/Cargo.toml crates/bot/
COPY crates/cutover/Cargo.toml crates/cutover/
COPY crates/store/Cargo.toml crates/store/
COPY crates/testsupport/Cargo.toml crates/testsupport/
COPY crates/voice-probe/Cargo.toml crates/voice-probe/
RUN mkdir -p src crates/core/src crates/discord/src crates/bot/src crates/cutover/src crates/store/src crates/testsupport/src crates/voice-probe/src \
    && echo '' > src/lib.rs \
    && echo 'fn main(){}' > crates/bot/src/main.rs \
    && echo '' > crates/core/src/lib.rs \
    && echo '' > crates/discord/src/lib.rs \
    && echo '' > crates/cutover/src/lib.rs \
    && echo '' > crates/store/src/lib.rs \
    && echo '' > crates/testsupport/src/lib.rs \
    && echo '' > crates/voice-probe/src/lib.rs \
    && echo 'fn main(){}' > crates/voice-probe/src/main.rs \
    && cargo fetch --locked

# Real sources; the release profile (opt-level=z, lto, strip) keeps the
# probe image small. Only the probe binary is built, not the bot.
COPY . .
RUN cargo build --release --locked -p two-bot-voice-probe

FROM gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2 AS runtime

# Non-root user: the probe never needs container root.
USER 65532:65532
WORKDIR /home/nonroot

COPY --from=builder --chown=65532:65532 /app/target/release/voice-probe ./voice-probe

# Echo mode listens here during probe runs only; the echo is never public
# in production. Probe mode makes outbound UDP and needs no ingress.
EXPOSE 9001/udp

ENTRYPOINT ["/home/nonroot/voice-probe"]
