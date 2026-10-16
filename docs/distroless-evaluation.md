# Distroless runtime evaluation ([TOG-12144](/TOG/issues/TOG-12144), follow-up of [TOG-10893](/TOG/issues/TOG-10893))

Historical evaluation. The cutover has adopted the digest-pinned distroless
runtime as the default image in `Dockerfile`. This document preserves the
original measurements and compatibility findings; the former trial image
(`Dockerfile.distroless`, retired in #1007) is not a separate build path.
The switch did not touch files under `wrangler/`.

Staging TLS-handshake receipt: deploy-staging run `38773972225` verified the
distroless head `46a30b19e` with `/health` 200 and `/readyz` 200 (all
components ready, gateway included) plus a green voice-synthetic pass — the
`rustls-platform-verifier` system-bundle path completes live Discord TLS.
Rollback requires a coordinated change, not a one-file restore: use a matched,
pinned bookworm builder and runtime in `Dockerfile` and update the
distroless-bound gates in the same PR: `dockerfile_parity_errors` in
`scripts/ci/test_toolchain_parity.py` for the builder pin,
`scripts/test-supply-chain.py` for the base-image pins and the runtime
assertions (no RUN, no apt-get/useradd//var/lib/dpkg, USER 65532),
`scripts/runtime-image-evidence.py` for the status.d layout (it rejects
var/lib/dpkg/status), and `scripts/vulnerability-preflight.py` for DISTRO. A
Bookworm rollback also requires a fresh CISO vulnerability disposition for the
measured HIGH/CRITICAL tuples: the prior 17-tuple decision expired on
2026-10-08 and does not carry over. Do not restore exceptions or add
`.trivyignore.yaml` entries until that disposition is recorded; then encode
only its approved, scoped, expiring exceptions there. In the same PR, update
both empty-file pins in `scripts/test-supply-chain.py`: the regex asserting
`vulnerabilities: []` and the active-content equality assertion. Replace them
with the approved entries and their reviewed evidence. Keep
`scripts/vulnerability-preflight.py` as a non-accepting tuple/provenance check:
it must bind the scan to the Bookworm distro and continue to report findings
as `not-accepted`; do not add exception-acceptance logic to the preflight.
Without a current disposition, keep the SBOM gate red and fix findings by
updating the base digest. Keep the Rust version aligned with
`rust-toolchain.toml`; do not mix trixie-built binaries with bookworm runtime
libraries. Require the parity guard, the sbom supply-chain lane, image
build/smoke, staging handshake, and the current CISO disposition for any
accepted tuples to pass before calling the rollback complete. SBOMs regenerate
per head through the supply-chain lane.

## Verdict

The default runtime is `gcr.io/distroless/cc-debian13:nonroot` (digest-pinned,
trixie builder). Do not use the `static` variant or deploy a `debug-nonroot`
variant. Per-precondition status: TLS handshake met per the cutover note above
(deploy-staging run `38773972225`); backup/guild-config upload-hook path still
open (needs sidecar/static upload binaries or host-side upload per
`docs/backup.md`; not proven on the distroless image); CISO acknowledgement of
the 14-package inventory and pin-refresh runbook still open (no receipt linked).

## Compatibility

- **CA certs:** the base ships Debian trust data (`ca-certificates`) at
  `/etc/ssl/certs/ca-certificates.crt`; nothing is installed or copied in
  except the release binary. Smoke verified the bundle is present and
  PEM-readable (docker-cp archive inspection of the built image). The live
  TLS handshake closed the remaining caveat: the `rustls-platform-verifier`
  path (`crates/discord/src/executor.rs`, `try_with_platform_verifier`)
  relies on the system bundle, and the staging receipt in the cutover note
  above proves it handshakes live Discord TLS on this base.
- **User:** base `nonroot` uid/gid 65532, workdir `/home/nonroot`,
  `COPY --chown=65532:65532`. CI verified PID 1 runs with non-zero
  real/effective/saved/filesystem uids (`docker top`).
- **Healthcheck:** exec-form `CMD ["/home/nonroot/two-bot", "--healthcheck"]`
  needs no shell. The probe is std+tokio only (`GET /health`, exit 0/1).
  CI: Docker HEALTHCHECK healthy, `/health` 200 `{"status":"ok"}`,
  `/readyz` 503 parked (gateway/database down, all six jobs parked,
  non-running, never started).
- **Boot/shutdown:** binary booted with no secrets or DB; SIGTERM exited 0
  in 0.236 s (10 s budget). The exec form keeps PID 1 semantics; the shutdown
  path is unchanged from bookworm (same binary). An earlier job-attempt-1
  drain timeout was superseded by the green job-attempt-2 rerun on the
  identical head.
- **No shell/tooling:** no shell, apt, dpkg, `getcap` or OpenSSL CLI.
  Triage via `docker cp`/`docker export` from the image, or a `debug-nonroot`
  variant locally only. Operator upload hooks (`TWO_BACKUP_UPLOAD_CMD`,
  `TWO_GUILD_CONFIG_UPLOAD_CMD` spawn external commands in
  `crates/bot/src/backup_cli.rs`) have no in-image shell utilities — cutover
  precondition: sidecar/static upload binaries or host-side upload.

## Size delta (uncompressed layer sum, CI-measured)

| Image | Run / head | Size | Binary |
|---|---|---|---|
| `debian:bookworm-slim` (historical default on main at the time) | [36951099822](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/36951099822) / `1c6481b6` (`sha256:715f…`) | 96,108,544 B (91.66 MiB) | 10,517,528 B (10.03 MiB) |
| `cc-debian13:nonroot` (trial) | [36948616604](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/36948616604) job attempt 2 / `9cc4815b` (`sha256:2bc5…`) | 42,807,296 B (40.82 MiB) | 10,518,416 B (10.03 MiB) |
| **Delta** | | **−53,301,248 B (−50.84 MiB, −55.5 %)** | +888 B (noise; same opt-z/LTO/strip profile) |

Both images fit the smoke budgets (112 MiB image, 11 MiB binary).

## CVE delta (Trivy v0.69.3, HIGH/CRITICAL incl. unfixed)

| Image | Finding |
|---|---|
| `debian:bookworm-slim` | 52 HIGH/CRITICAL (16 conditional tuples from the original 17-tuple CISO disposition; one CVE-2026-78408 mount tuple was later retired by removal; the remaining conditions expired 2026-10-08; 36 unaccepted; gate red) — run `36942267564` (scan job failed, suppression 0) |
| `debian:trixie-slim` Step A | left HIGH/CRITICAL rows with no CISO disposition, superseded by the pre-approved Step B ([TOG-11974](/TOG/issues/TOG-11974) CEO decision) — exact counts not re-verified from reachable logs for this card |
| `cc-debian13:nonroot` trial | **0 HIGH/CRITICAL** in both Rust-workspace and image gates; 14 deb packages; preflight bound with 0 suppressed — run `36948616604` |

Counts are point-in-time (CI re-measures every head), not a standing PASS.
The distroless `nonroot` tag is unversioned: refresh the digest pin by hand
after a Debian point release if no Dependabot bump arrives.

## Why `cc`, not `static`

The release binary dynamically links glibc/`libgcc_s`/`libstdc++` (ring crypto,
hyper/tokio). The `static` variant ships no libc — it would require retargeting
to musl, out of scope. `cc` provides exactly the needed userspace and nothing
else (14 packages, no shell or package manager).

## Cutover preconditions — status at closeout

1. [MET] Staging E2E TLS handshake on the distroless image (deploy-staging run
   `38773972225` in the cutover note above).
   [OPEN] Backup/guild-config upload-hook path on the distroless image —
   needs sidecar/static upload binaries or host-side upload (see
   `docs/backup.md`); not proven here.
2. [OPEN] CISO acknowledgement of the 14-package inventory and pin-refresh
   runbook — no receipt linked.
3. [KEPT] Keep the shell-free smoke probes (`docker cp`, `docker top`, binary
   `--healthcheck`); never ship `debug-nonroot`.

## Runtime profile: cold start, RSS and readiness timings

Historical trial-image profiling for the cutover guard. The default
`Dockerfile` builds the pinned `cc-debian13:nonroot` base digest; current
build-provenance arguments and labels do not alter the runtime. This profile
remains historical evidence. No guard logic changes here; thresholds stay
where the cutover guard reads them in the
[production signal thresholds](production-deploy.md#signal-thresholds-budgets-and-rollback-triggers).

### Repeatable procedure

Run on a Docker-capable host (self-hosted runner or dev machine), never the
controller. No secrets, database or Discord traffic are used at any step.

```sh
docker buildx build --load --platform linux/amd64 \
  -t two-bot:distroless-profile -f Dockerfile .
python3 scripts/container-smoke.py two-bot:distroless-profile
```

Cold start, RSS peak and shutdown (parked mode, same 256 MiB cap as smoke):

```sh
docker run -d --name prof --memory 256m \
  --publish 127.0.0.1::8080 two-bot:distroless-profile
# Poll GET /health until 200 {"status":"ok"}; record start-to-200 as T_health.
# Poll GET /readyz until the parked 503 breakdown; record start-to-503 as T_readyz.
# Every second until healthy, sample RSS with:
docker stats --no-stream --format '{{.MemUsage}}' prof
# Record the maximum sample as the RSS peak, then:
docker kill --signal TERM prof
time docker wait prof
# Expect exit 0; record the elapsed time as T_shutdown.
docker rm --force prof
```

Poll with a hand-rolled client or `curl --max-time` and never follow redirects,
so one healthy endpoint cannot masquerade as another (same contract as
`scripts/container-smoke.py`).

### Measured numbers

Every row below is CI-anchored; this slice ran no Docker build (no Docker in the
authoring workspace) and records no fresh measurement. Rerun the procedure
above for exact seconds on a new head.

| Signal | Trial measurement | Source | Cutover reading |
|---|---|---|---|
| Image size | 42,807,296 B (40.82 MiB); binary 10,518,416 B (10.03 MiB); −53,301,248 B (−55.5%) versus the bookworm default at that time | Trial check run `36948616604`, attempt 2 | Fits the 112 MiB image / 15 MiB binary ceilings with headroom |
| Cold start to `/health` 200 | Bounded by the 30 s smoke deadline; exact seconds not in reachable logs | Smoke contract | Well inside the 60 s first-200 staging budget |
| `/readyz` parked answer | 503 with process ready and gateway down (6-job parked map at the trial head; 10-job map on current heads) | Smoke contract | Truthful parked, never acceptance |
| Docker HEALTHCHECK | Healthy within 30 s of start | Smoke contract | Same deadline as `/health` |
| SIGTERM shutdown | Exit 0 in 0.236 s (trial); 0.095 s historical default image | Trial log; baseline doc | Inside the 10 s smoke and 35 s drain budgets |
| Parked-mode RSS peak | Not sampled by smoke. Proxies: synthetic pipeline peak ≈ 24 MiB (debug, mock workload); B1 floor ≈ 140 MiB; `lite` gate signal ≈ 200 MiB on the shipped `basic` placement; no OOM at the 256 MiB cap | Baseline and benchmark docs | Placement unchanged; loaded-guild RSS is a separate placement question, not a numeric B2 acceptance criterion or authority to change placement |
| Gateway-connected first 200 | Never measured on the trial (no secrets by design) | Staging readiness workflow | The 60 s restart/deploy-to-first-200 budget is a readiness interval, not B2 outage-start-to-verified-recovery |

### SLO thresholds

No threshold changes. The existing operational budgets already bound this runtime:
first 200 within 60 s of a restart or deploy event (a readiness workflow
interval, not B2 outage recovery), SIGTERM drain inside 35 s, RSS observations
against the B1 floor for separate placement review (not B2 pass/fail), and the
image/binary size ceilings. The cutover guard keeps reading those values; this
section is the distroless evidence behind them.

Not claimed here: backup/guild-config upload-hook paths or loaded-guild RSS —
those stay on the cutover preconditions above. The live Discord TLS handshake
is claimed in the cutover note above (deploy-staging run `38773972225`).
