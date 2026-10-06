# Cutover container image inventory (offline, read-only)

Read-only record of the cutover runtime image. No registry push, no deploy,
no secret access. Values come from static files at the baseline inventory
commit, except for the separately sourced builder-row refresh below. No image
was built or pulled for this note; the refresh adds no measured evidence.

- Baseline inventory commit: `7797b1e166d2aa7b28725cdddc1b8f0c097a5ed5` (2026-10-03)
- Builder-row refresh (2026-10-04): [`Dockerfile:10` at `723663df9c74fd24734170a981cf59908aadd33a`](https://github.com/TogetherWeOwn/two-bot-next/blob/723663df9c74fd24734170a981cf59908aadd33a/Dockerfile#L10).
  The baseline used `rust:1.94-trixie`; only the builder row now records the
  Rust 1.98 tag and digest. Runtime, layer, SBOM and size notes remain the
  baseline record, with unmeasured values still TBD.
- Source files: `Dockerfile`, `.github/workflows/sbom.yml`,
  `docs/supply-chain.md`, `scripts/container-smoke.py`, `.trivyignore.yaml`
- Method: read the files above only. Byte sizes are TBD until the CI-built
  image for the cutover candidate is inspected.

## Base images

| Stage | Reference (pinned) | OS | Purpose |
|---|---|---|---|
| builder | `rust:1.98-trixie@sha256:a8a5f0a1e5fe7dfe1d352591e4a1c7dd2c08fd70475cae872cf3458ba0df0546` | Debian 13 (trixie) | Full toolchain; compiles the release binary, then discarded |
| runtime | `gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2` | Debian 13, distroless `cc` | Ships glibc, libgcc, CA trust data and the `nonroot` account (uid/gid 65532); carries only the release binary |

Both stages are Debian 13, so the binary links against the glibc it runs on.
Tags stay readable for automated Docker updates while the digests make both
stages immutable. There is no build-arg override for either base.

## Runtime layer breakdown (static, sizes TBD)

The shipped image is the `runtime` stage only. Builder layers are cache
scaffolding and never ship. Exact byte counts need `docker history` /
`docker inspect` against the CI-built image and were not measured offline.

| # | Dockerfile step | Ships a layer? | Notes |
|---|---|---|---|
| R1 | `FROM gcr.io/distroless/cc-debian13:nonroot@sha256:e792…` | Base layers (count/size TBD) | Debian trust bundle plus 14 documented OS packages; no shell, package manager, or OpenSSL CLI |
| R2 | `LABEL org.opencontainers.image.revision`, `com.togetherweown.build-id` | Metadata only | Non-secret build provenance; zero bytes |
| R3 | `USER 65532:65532`, `WORKDIR /home/nonroot` | Metadata only | Config, no filesystem delta |
| R4 | `COPY --from=builder /app/target/release/two-bot ./two-bot` | Yes, one file layer (size TBD) | The only added content: the stripped release binary (`opt-level=z`, LTO, stripped). Budget: 15 MiB per `scripts/container-smoke.py` |
| R5 | `EXPOSE 8080`, `ENV LISTEN_ADDR=0.0.0.0:8080` | Metadata only | Liveness/readiness port and bind address |
| R6 | `HEALTHCHECK CMD ["/home/nonroot/two-bot", "--healthcheck"]` | Metadata only | Exec-form probe; needs no shell |
| R7 | `ENTRYPOINT ["/home/nonroot/two-bot"]` | Metadata only | Exec form so PID 1 receives SIGTERM |

Effective layer delta over the distroless base: one file layer (R4).
Builder-only steps (`COPY` manifests, `cargo fetch`, `COPY . .`,
`cargo build --release --locked`) affect build cache, not the shipped layer
count. Size gate for the whole runtime image: 112 MiB per
`scripts/container-smoke.py`; release profile targets the 256 MiB `lite`
ceiling from ADR 0001.

## SBOM tool output location

No SBOM file is committed to the repo. Per-image SBOMs are produced by the
reusable `sbom.yml` workflow (called from `check.yml` on PRs and from
`release.yml` on releases) and live in CI artifacts:

- Scanner: Trivy Action 0.35.0 with Trivy v0.69.3, fresh vulnerability DB per
  run, no shared layer/DB cache, `cache: false` everywhere.
- `sbom/rust-workspace.cdx.json`: CycloneDX inventory of `Cargo.lock`
  (workspace, optional and dev dependencies; not a claim every locked crate
  links into the binary).
- `sbom/container-image.cdx.json`: CycloneDX inventory of the runtime image
  (OS/image packages; Rust binaries are not reliably recoverable by image
  analysis, hence the separate lockfile scan).
- `sbom/rust-vulnerabilities.json`, `sbom/image-vulnerabilities.json`: gate
  reports, fail on HIGH/CRITICAL including unfixed findings; exceptions only
  via narrowly scoped `.trivyignore.yaml` entries (currently empty:
  `vulnerabilities: []`).
- `sbom/source-sha.txt`, `sbom/image-id.txt`, `SHA256SUMS`: provenance binding
  the BOMs to the exact source commit and immutable image ID.
- `sbom/runtime-image-evidence.json`, `sbom/vulnerability-preflight.json`
  (plus `.sha256` checksums): exact-image filesystem evidence and
  finding-to-installed-tuple binding; diagnostic only, never a waiver.
- Retention: `supply-chain` Actions artifact, 14 days, kept even on gate
  failure. On releases, the two `.cdx.json` files plus `SHA256SUMS`,
  `source-sha.txt` and `image-id.txt` attach to the GitHub Release only after
  both gates and inventory validation pass.

Fetch the SBOMs for a cutover candidate run (no deploy secrets needed):

```sh
gh run download <run-id> -n supply-chain -D <run-owned-scratch-directory>
```

Status for this note: TBD per-image. No `supply-chain` artifact was
downloaded for this offline inventory, so there is no SBOM pointer to a
specific run yet. The cutover evidence manifest should name the
candidate's run ID and attach or link its two `.cdx.json` files.

## Baseline stale-vs-main notes

These comparisons describe the 2026-10-03 baseline, not the builder refresh
or the current PR diff.

- At the baseline inventory commit, `Dockerfile` was identical to the then-current
  `origin/main` (empty `git diff origin/main -- Dockerfile`). Nothing was stale.
- That baseline branch-vs-main diff touched bot/voice/watch/docs code, not the
  image: no Dockerfile, base digest, release profile, or SBOM workflow change
  rode with that branch. The separately sourced builder refresh changes the
  builder tag and digest, not the runtime image or its measurement status.
- `Dockerfile.distroless` is a local-trial-only variant, never built by CI
  and never the cutover image. It omits the provenance `ARG`/`LABEL` block
  the CI `Dockerfile` carries, so do not substitute one for the other when
  collecting cutover evidence.
- Runtime is Debian 13 distroless (`cc-debian13:nonroot`); the older Bookworm
  notes and conditional dispositions in `docs/supply-chain.md` do not carry
  over. Automated Docker updates track both pinned references; the unversioned
  `nonroot` tag needs a manual digest refresh if no bump arrives after a
  Debian point release.
- Trust data is the base image's own `ca-certificates` bundle; nothing is
  installed or copied into the runtime except the release binary. The bot runs
  as `nonroot` and its only in-image exec is its own `--healthcheck`.

## What remains for cutover readiness

1. Build the merged cutover candidate in CI and record its immutable image ID.
2. Download that run's `supply-chain` artifact; file the two `.cdx.json`
   names, `SHA256SUMS`, and both gate results in the cutover evidence
   manifest.
3. Run `docker history` / `docker inspect` on the recorded image ID to fill
   in the TBD byte counts for R1 and R4 and confirm the 112 MiB / 15 MiB
   budgets before pinning the deployment digest.
