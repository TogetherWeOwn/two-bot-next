# two-bot-next: single always-on Cloudflare Container (ADR 0001).
# Multi-stage: the builder needs the full Rust toolchain; the runtime image
# carries the release binary on distroless/base-nossl (glibc, CA trust data)
# plus official Trixie libgcc-s1/gcc-14-base from the builder. No zlib, shell,
# package manager or OpenSSL CLI. rustls uses platform/webpki
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
# Non-secret build provenance, compiled into readiness (not a runtime override).
ARG BOT_BUILD_REVISION=unknown
ARG BOT_BUILD_ID=unknown
ARG BOT_DEPLOY_ID=unknown
RUN cargo build --release --locked

# Compose only the GCC runtime needed by the binary, from the immutable
# official Trixie builder. Keep native package/source/version metadata for
# scanners, with native checksums for only the copied files; no package is
# installed, modified or relabelled.
RUN . /etc/os-release && test "$VERSION_CODENAME" = trixie \
    && mkdir -p /runtime-root/var/lib/dpkg/status.d \
    && for package in libgcc-s1 gcc-14-base; do \
         test "$(dpkg-query -W -f='${Status}' "$package")" = 'install ok installed' \
         && test "$(dpkg-query -W -f='${source:Package}' "$package")" = gcc-14 \
         && dpkg-query -s "$package" > "/runtime-root/var/lib/dpkg/status.d/$package" \
         && cp -L --parents "/usr/share/doc/$package/copyright" /runtime-root/ \
         || exit 1; \
       done \
    && libgcc="$(dpkg-query -L libgcc-s1 | grep '/libgcc_s.so.1$')" \
    && test -f "$libgcc" \
    && cp -L --parents "$libgcc" /runtime-root/ \
    && for package in libgcc-s1 gcc-14-base; do \
         dpkg-query --control-show "$package" md5sums > "/runtime-$package.md5sums" \
         && while read -r checksum path; do \
              if test -f "/runtime-root/$path"; then printf '%s  %s\n' "$checksum" "$path"; fi; \
            done < "/runtime-$package.md5sums" > "/runtime-root/var/lib/dpkg/status.d/$package.md5sums" \
         && test -s "/runtime-root/var/lib/dpkg/status.d/$package.md5sums" \
         || exit 1; \
       done \
    && (cd /runtime-root && md5sum -c var/lib/dpkg/status.d/*.md5sums)

# The base provides glibc, CA trust data and uid/gid 65532. Add only the
# official libgcc library, copyright files and truthful status.d records.
FROM gcr.io/distroless/base-nossl-debian13:nonroot@sha256:e6b0f82595aa1edde72c8054de7a9dca30f0f8f54820c37a167c0ddfc21c5c67 AS runtime

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

COPY --from=builder /runtime-root/ /
COPY --from=builder --chown=65532:65532 /app/target/release/two-bot ./two-bot

# Liveness + readiness (also the DO keepalive targets, see wrangler/).
EXPOSE 8080
ENV LISTEN_ADDR=0.0.0.0:8080 LOG_FORMAT=json

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/home/nonroot/two-bot", "--healthcheck"]

ENTRYPOINT ["/home/nonroot/two-bot"]
