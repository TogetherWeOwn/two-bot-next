# two-bot-next: single always-on Cloudflare Container (ADR 0001).
# Multi-stage: the builder needs the full Rust toolchain; the runtime image
# carries only the release binary on distroless/cc (glibc, libgcc, CA trust
# data; no shell, package manager or OpenSSL CLI). rustls uses platform/webpki
# roots, not OpenSSL. PID 1 receives the Container SIGTERM via the exec form.
#
# Multi-platform manifest digests keep tag names readable for Dependabot while
# making both stages immutable. The builder and distroless runtime are both
# Debian 13 (trixie), so the binary links against the same glibc it runs on.
FROM rust:1.99-trixie@sha256:09027f873d45b8ecd1dc86f1fa13b7a62b875c961f017a0e331267b350430137 AS builder

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

# Real sources; the release profile (opt-level=z, lto, strip) targets the
# `lite` 256 MiB ceiling from ADR 0001.
COPY . .
# Embed native .dep-v0 crate metadata, including in the stripped release binary.
# Install only the verified executable, never run the archive's installer.
RUN set -eu; \
    case "$(uname -m)" in \
      x86_64) arch=x86_64; sha=7676443367c5e33d9fb22acef63cccb5e9202de12dae1b0579995eb3e2bc7a18 ;; \
      aarch64) arch=aarch64; sha=c324af56990601354bbf40080f582ba46f8ec8b807a1388e25bb7e429d285c18 ;; \
      *) exit 1 ;; \
    esac; \
    mkdir /opt/auditable; \
    curl --fail --location --silent --show-error --max-time 120 \
      "https://github.com/rust-secure-code/cargo-auditable/releases/download/v0.7.7/cargo-auditable-${arch}-unknown-linux-musl.tar.xz" \
      -o /opt/auditable/tool.tar.xz; \
    printf '%s  %s\n' "$sha" /opt/auditable/tool.tar.xz | sha256sum --check; \
    tar -xJf /opt/auditable/tool.tar.xz -C /opt/auditable --strip-components=1 \
      "cargo-auditable-${arch}-unknown-linux-musl/cargo-auditable"; \
    install -m 0755 /opt/auditable/cargo-auditable /usr/local/cargo/bin/cargo-auditable; \
    rm -rf /opt/auditable

# Non-secret build provenance, compiled into readiness (not a runtime override).
ARG BOT_BUILD_REVISION=unknown
ARG BOT_BUILD_ID=unknown
ARG BOT_DEPLOY_ID=unknown
RUN cargo auditable build --release --locked

# The runtime base already ships Debian trust data (ca-certificates) and the
# nonroot account (uid/gid 65532, home /home/nonroot); nothing is installed.
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:3ee00291cf386ec52a552e6a858d555debd74776eb4bfa53fa3df2cd6677515e AS runtime

# Non-secret build provenance (main #137/#156), recorded as image labels.
ARG BOT_BUILD_REVISION=unknown
ARG BOT_BUILD_ID=unknown
ARG BOT_DEPLOY_ID=unknown
LABEL org.opencontainers.image.revision=$BOT_BUILD_REVISION \
      com.togetherweown.build-id=$BOT_BUILD_ID \
      com.togetherweown.deploy-id=$BOT_DEPLOY_ID

# Non-root user: the bot never needs container root.
USER 65532:65532
WORKDIR /home/nonroot

COPY --from=builder --chown=65532:65532 /app/target/release/two-bot ./two-bot

# Liveness + readiness (also the DO keepalive targets, see wrangler/).
EXPOSE 8080
ENV LISTEN_ADDR=0.0.0.0:8080 LOG_FORMAT=json

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/home/nonroot/two-bot", "--healthcheck"]

ENTRYPOINT ["/home/nonroot/two-bot"]
