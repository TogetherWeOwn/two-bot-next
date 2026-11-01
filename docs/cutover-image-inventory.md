# Cutover container image inventory (measured)

Read-only record of the cutover runtime image. No registry push, no deploy,
no secret access. Static rows come from the `Dockerfile` at sample source SHA
`4d183937`; byte counts below are measured from a CI-built sample image of
that main head, described under Measured sample image.

- Baseline inventory commit: `7797b1e166d2aa7b28725cdddc1b8f0c097a5ed5` (2026-10-03)
- Builder-row refresh (2026-11-01): [`Dockerfile:10` at `e403836cec544d6a1c4f74c6fa0c8daebcf91734`](https://github.com/TogetherWeOwn/two-bot-next/blob/e403836cec544d6a1c4f74c6fa0c8daebcf91734/Dockerfile#L10).
  The baseline used `rust:1.94-trixie`; only the builder row now records the
  Rust 1.99 tag and digest. Runtime, layer, SBOM and size notes below are measured on a CI-built
  main-head sample, not the cutover candidate and not the baseline estimate.
- Source files: `Dockerfile`, `.github/workflows/sbom.yml`,
  `docs/supply-chain.md`, `scripts/container-smoke.py`, `.trivyignore.yaml`
- Method: files above plus a CI-built sample image of main head (build logs,
  `supply-chain` artifact, saved `image.tar`). No image was built or pulled
  offline for this note.

## Base images

| Stage | Reference (pinned) | OS | Purpose |
|---|---|---|---|
| builder | `rust:1.99-trixie@sha256:931c2c0531a8b143e044f0f3914dfe802f15bb3d1955f0b657e9359327ac2ffc` | Debian 13 (trixie) | Full toolchain; compiles the release binary, then discarded |
| runtime | `gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2` | Debian 13, distroless `cc` | Ships glibc, libgcc, CA trust data and the `nonroot` account (uid/gid 65532); carries only the release binary |

Both stages are Debian 13, so the binary links against the glibc it runs on.
Tags stay readable for automated Docker updates while the digests make both
stages immutable. There is no build-arg override for either base.

## Runtime layer breakdown (measured)

The shipped image is the `runtime` stage only. Builder layers are cache
scaffolding and never ship. Byte counts are measured from the CI-built
sample image named under Measured sample image.

| # | Dockerfile step | Ships a layer? | Notes |
|---|---|---|---|
| R1 | `FROM gcr.io/distroless/cc-debian13:nonroot@sha256:e792…` | 21 base layers, 29,128,192 bytes (27.78 MiB) as saved-tar blobs | Debian trust bundle plus 14 documented OS packages; no shell, package manager, or OpenSSL CLI |
| R2 | `LABEL org.opencontainers.image.revision`, `com.togetherweown.build-id` | Metadata only | Non-secret build provenance; zero bytes |
| R3 | `USER 65532:65532`, `WORKDIR /home/nonroot` | One empty layer (no tar entries), 1,024 bytes as saved-tar blob; `USER` is config-only | Config; the layer is empty because `/home/nonroot` already exists in base layer 7 |
| R4 | `COPY --from=builder --chown=65532:65532 /app/target/release/two-bot ./two-bot` | Yes, one file layer, 16,065,536 bytes (15.32 MiB) as saved-tar blob; file itself 16,062,800 bytes (15.32 MiB) | The only added file content: the stripped release binary (`opt-level=z`, LTO, stripped). Budget: 18 MiB per `scripts/container-smoke.py` (recalibrated 2026-10-24; the 2026-10-11 sample below predates it) |
| R5 | `EXPOSE 8080`, `ENV LISTEN_ADDR=0.0.0.0:8080 LOG_FORMAT=json` | Metadata only | Liveness/readiness port, bind address and JSON log format |
| R6 | `HEALTHCHECK CMD ["/home/nonroot/two-bot", "--healthcheck"]` | Metadata only | Exec-form probe; needs no shell |
| R7 | `ENTRYPOINT ["/home/nonroot/two-bot"]` | Metadata only | Exec form so PID 1 receives SIGTERM |

Effective layer delta over the distroless base: two layers, an empty
`WORKDIR` layer (R3, 1,024 bytes, no tar entries) and the binary (R4, one
file layer).
Builder-only steps (`COPY` manifests, `cargo fetch`, `COPY . .`,
`cargo build --release --locked`) affect build cache, not the shipped layer
count. Size gate for the whole runtime image: 112 MiB per
`scripts/container-smoke.py`; release profile targets the 256 MiB `lite`
ceiling from ADR 0001.

## Measured sample image

Built and measured in hosted CI on the merged main head, not offline. This is
a sample of main, not the pinned cutover candidate.

- Source SHA: `4d183937e22105675fe4d876f5d5249d81f14bf7` (main, 2026-10-11)
- Check run: [38155009694](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/38155009694);
  container smoke job: [114516108721](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/38155009694/job/114516108721)
- Image ID (`sbom/image-id.txt`, from the `sbom.yml` build): `sha256:2257060a144068838731183bf151bc5ffb2a7aec8beff6dbb1e20fc90a859813`
  (`amd64`/`linux`, user `65532:65532`, entrypoint `/home/nonroot/two-bot`)
- Budget metric (gate): summed uncompressed `docker history` layer bytes
  `43,772,940` bytes (41.75 MiB) against `117,440,512` bytes (112 MiB) — PASS.
  Headroom `73,667,572` bytes (70.25 MiB, 62.7%). This was measured on the
  container smoke job's own build of the same source SHA, a separate build
  from the `sbom.yml` build above. This note does not record that build's
  image ID, so the figure is not measured on `sha256:2257060a…`.
- Layer blobs of image `sha256:2257060a…`: the saved `image.tar` totals
  `45,194,752` bytes (see the saved-tar diagnostic), `72,245,760` bytes under
  the same 112 MiB budget.
- Binary metric (gate): release binary `16,062,800` bytes (15.32 MiB) against
  `16,777,216` bytes (16 MiB) — PASS. Headroom `714,416` bytes (0.68 MiB,
  4.3%). The binary exceeds the old 15 MiB ceiling by `334,160` bytes, so the
  16 MiB tripwire in `scripts/container-smoke.py` was required at the time
  (recalibrated to 18 MiB on 2026-10-24).
- Cross-check: `sbom/runtime-image-evidence.json` (schema 2, `complete: true`)
  records the same source SHA, image ID, and binary size `16,062,800` with
  SHA-256 `4d41c4fb7fbfa1d7d6752a0503491a2b6672407a1d4488107fb8ead9b764116a`,
  14 installed packages, no setid files, no file capabilities, no shell or
  package-manager tools.
- Saved-tar diagnostic (`sbom-partial` `image.tar`, 23 layers): R1 base blobs
  0–20 sum `29,128,192` bytes (27.78 MiB); WORKDIR blob `1,024` bytes; R4
  binary blob `16,065,536` bytes (15.32 MiB, `2,736` bytes tar overhead over
  the file); total blobs `45,194,752` bytes (43.10 MiB). The `1,421,812`-byte
  gap to the history-sum budget metric is tar overhead and accounting, not a
  second budget.

## SBOM tool output location

No SBOM file is committed to the repo. Per-image SBOMs are produced by the
reusable `sbom.yml` workflow (called from `check.yml` on PRs and from
`release.yml` on releases) and live in CI artifacts:

- Scanner: Trivy Action 0.36.0 with Trivy v0.69.3, fresh vulnerability DB per
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

Status for this note: sample measurement of main head, not the cutover
candidate. The `supply-chain` artifact for run `38155009694` holds
`container-image.cdx.json`, `rust-workspace.cdx.json`, `SHA256SUMS`,
`source-sha.txt`, `image-id.txt`, `runtime-image-evidence.json`, and
`vulnerability-preflight.json` for source SHA
`4d183937e22105675fe4d876f5d5249d81f14bf7` and image ID
`sha256:2257060a144068838731183bf151bc5ffb2a7aec8beff6dbb1e20fc90a859813`.
It expires 2026-10-25; the `sbom-partial` artifact (`image.tar`) expires
2026-10-12. Fetch `supply-chain` before then with
`gh run download 38155009694 -n supply-chain -D <run-owned-scratch-directory>`.

## Baseline stale-vs-main notes

These comparisons describe the 2026-10-03 baseline, not the builder refresh
or the current PR diff.

- At the baseline inventory commit, `Dockerfile` was identical to the then-current
  `origin/main` (empty `git diff origin/main -- Dockerfile`). Nothing was stale.
- That baseline branch-vs-main diff touched bot/voice/watch/docs code, not the
  image: no Dockerfile, base digest, release profile, or SBOM workflow change
  rode with that branch. The separately sourced builder refresh changes the
  builder tag and digest, not the runtime image or its measurement status.
- The distroless runtime is now the default image in `Dockerfile`; its
  provenance arguments and labels are part of the cutover image build. Earlier
  trial measurements remain historical evidence, not a separate build path.
- Runtime is Debian 13 distroless (`cc-debian13:nonroot`); the older Bookworm
  notes and conditional dispositions in `docs/supply-chain.md` do not carry
  over. Automated Docker updates track both pinned references; the unversioned
  `nonroot` tag needs a manual digest refresh if no bump arrives after a
  Debian point release.
- Trust data is the base image's own `ca-certificates` bundle; nothing is
  installed or copied into the runtime except the release binary. The bot runs
  as `nonroot` and its only in-image exec is its own `--healthcheck`.

## What remains for cutover readiness

The numbers above come from sample run 38155009694 on main head, not from the
pinned cutover candidate, and the sample artifacts expire (see Status above).
Before the deployment digest is pinned:

1. Build the merged cutover candidate in CI on a push to main, so the container
   smoke job runs, and record its immutable image ID and source SHA.
2. Download that run's `supply-chain` artifact; file the two `.cdx.json`
   names, `SHA256SUMS`, and both gate results under the Releases receipt of
   the B4 evidence manifest (`docs/cutover.md` B4). That receipt does not list
   SBOMs yet, so add them there.
3. Re-measure on the candidate's image ID, and confirm the 112 MiB / 18 MiB
   budgets pass. CI keeps the candidate image only as the
   `sbom-partial-<run_id>-<attempt>` artifact (`image.tar`, 1-day retention),
   so download it within that window. R1 and R4 are saved-tar layer-blob
   sizes: read them from the archive's `manifest.json` layers, not from
   `docker history`. The budget metric is summed `docker history` layer bytes,
   so `docker load` the archive for that. The container-smoke job log of the
   same push-to-main run gives the budget lines, but for that job's own build,
   not the image ID. Pinning the deployment digest is tracked separately and
   is not part of this inventory note.
