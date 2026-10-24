# TOG-9694 — B1 baseline measurement (Node discord.js bot)

Date: 2026-09-29. Method: real `two-bot` process (unmodified `src/index.ts`),
mock-discord gateway double, scratch Postgres on agent-testdb.
No production services touched. No docker available in this env — no
`--memory=256m` cgroup cap; numbers below are unconstrained RSS, which is a
conservative (high) bound for the lite gate.

## Load profile

- 180 s idle → 480 s scripted load → 120 s settle (13 min total).
- Load: 1,199 gateway dispatches at ~2.5/s (400 joins + 400 messages + 399
  voice-state) — roughly half a day of guild traffic compressed into 8 min.
- DB proof the pipeline ran: `events` = 1,998 rows
  (400 member_join + 400 gate_cleared + 400 first_message + 399
  voice_session_start + 399 first_voice_session), `members` = 1,199 rows.
- Bot exited 0 on SIGTERM after the run.

## RSS / CPU (sampled every 5 s from /proc, % of one core)

| phase  | n  | RSS min | RSS p50 | RSS p95 | RSS max | CPU p50 | CPU p95 | CPU max |
|--------|----|---------|---------|---------|---------|---------|---------|---------|
| idle   | 35 | 133     | 153     | 153     | 153     | 0.0%    | 0.2%    | 0.8%    |
| load   | 97 | 133     | 140     | 141     | 142     | 0.4%    | 0.8%    | 1.2%    |
| settle | 24 | 138     | 138     | 138     | 138     | 0.0%    | 0.0%    | 0.2%    |

(All MiB.)

Caveats: mock guild has 3 members, no cache pressure from a real 107-member
guild, no privileged member-chunk payloads, no TLS to Discord, pool max 5 on a
local test DB. Real-guild RSS will be *higher* — cache and chunk buffers scale
with member count — so treat ~140 MiB as a floor, not a ceiling.

## Gate verdict: `basic` (safe default)

Peak measured RSS ≈ 153 MiB < 200 MiB nominally passes the lite gate — but the
mock guild is ~3% of the real member count and discord.js caches (members,
messages, voice states) grow with it. TOG-9408 already flagged "RSS likely
>256 MiB → moves to basic ≈ $7–9". One OOM-kill of the gateway drops joins;
the $5.30/mo lite→basic step is not worth that risk. **Ship `basic`.** Any
later placement decision requires separate review; B2's four-ACTIVE-hour
evidence does not establish numeric RSS acceptance criteria or authorize a
placement change.

## Cost (Cloudflare Container pricing, per TOG-9408/ADR-0001 model)

| Setup | Memory | Disk | CPU | Total/mo |
|---|---|---|---|---|
| Node on **`basic`** (1 GiB·4 GB) | $6.26 | $0.68 | $0 | **≈ $6.9** |
| Node on `lite` (if a later staging soak proves RSS) | $1.40 | $0.31 | $0 | ≈ $1.7 |
| Rust/twilight on `lite` (S2 prototype to confirm) | $1.40 | $0.31 | $0 | ≈ $1.7 |

Rust delta vs Node-on-basic: ≈ $5.2/mo (≈ $63/yr) — same as ADR 0001. Honest
caveat stands: $0 company-bill delta while the Coolify VPS exists.

## Events/day estimate (no prod DB read — credential absent in this env)

Guild: ~84 humans / 107 members. Bot subscribes to 11 gateway event families
(joins, messages+reactions, voice, invites, audit log, interactions). Model:
~1.5k–3.5k gateway dispatches/day (presence/typing noise excluded; Discord
does not send those for these intents). At 2.5 dispatches/s in this soak, one
day of traffic ≈ 10–25 min of processing; CPU is noise either way (<1% of even
the 1/16-vCPU lite slice). This remains a historical planning estimate, not a B2
acceptance criterion. The four-ACTIVE-hour policy supersedes the former seven-day
plan; any later verified counts belong in a separate record and do not replace
this estimate retroactively.

## Container keepalive verdict: DO-alarm keepalive REQUIRED

Verified against `@cloudflare/containers@0.3.7` source (`dist/lib/container.js`)
plus the Workers Containers architecture doc:

- Default `sleepAfter` is 10 min; `onActivityExpired()` default calls `stop()`
  (SIGTERM → 15-min grace → SIGKILL).
- The inactivity clock (`sleepAfterMs`) is renewed **only** by inbound request
  paths (`containerFetch` proxy, `decrementInflight` reaching 0) and explicit
  `renewActivityTimeout()` calls — 10 call sites, all on the DO fetch/proxy or
  schedule path. Nothing in the container's outbound traffic (including the
  Discord gateway WebSocket) touches it.
- `isActivityExpired()` returns true with zero in-flight requests once the
  deadline passes, regardless of how busy the gateway socket is.

So the top risk from TOG-9408 is confirmed: an always-on Discord gateway —
pure outbound WS, zero inbound HTTP — **will be slept after `sleepAfter` with
no warning to the bot**, silently dropping joins/voice events. The keepalive is
not optional.

Required design (matches the S1 `wrangler/` skeleton on
`feat/s1-cargo-scaffold-container-skeleton`): a self-perpetuating DO
`schedule()` tick (not a raw `alarm()` override — the Container base class owns
`alarm()`) every ≤60 s that calls `renewActivityTimeout()` and probes the
container's `/readyz`. Belt-and-braces `sleepAfter = "30m"` so a missed tick or
two never costs the session. Inbound `/health` + `/readyz` fetches renew
activity implicitly. This needs proving on a real deployment (B2): deploy the
skeleton with a scratch Worker, hold the gateway open with zero inbound
traffic for >30 min, assert no `onActivityExpired` fires and no RESUME gap
appears. A live-network scratch deploy was not possible from this sandbox
(no wrangler, no outbound CF API writes attempted — read-only verify only).

## Rust runtime image PR gate

The independent `container smoke` job in `.github/workflows/check.yml` builds
this repository's Dockerfile on `linux/amd64`, loads it into local Docker,
and uses BuildKit's `gha` cache. It now uses the self-hosted CI runner (the
original measurement below used a hosted runner). It never pushes an image or
receives deployment credentials. Automatic pull-request CI is the validation
route; no workflow dispatch is needed for this repair. The existing required
`check` job is unchanged.

`scripts/container-smoke.py` prints both sizes in bytes and MiB to the log and
job summary and fails above these calibrated ceilings. The measurements and
headroom below are historical, not measurements of the current PR head:

| Artifact | Metric / provenance | Measured | Maximum | Headroom |
|---|---|---|---|---|
| Runtime image | Historical Docker image inspect `Size` from 2026-09-30 PR #78 (uncompressed layers, not registry transfer size) | 87.19 MiB / 91,429,497 bytes | 112 MiB / 117,440,512 bytes | 24.81 MiB / 28.4% |
| Release binary | `stat` of `/home/two-bot/two-bot` in the final image, event-executors head | 15.04 MiB / 15,770,928 bytes (2026-10-10; [event-executors CI smoke](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/38062362788/job/114254309361)) | 16 MiB / 16,777,216 bytes | 0.96 MiB / 6.4% |
| Release binary — automation-only head before event-executor merge | `stat` of `/home/two-bot/two-bot` in the final image | 15.04 MiB / 15,769,872 bytes | 16 MiB / 16,777,216 bytes | 0.96 MiB / 6.0% |

The runtime-image row above is the historical inspect `Size` result, not the
current gate metric. The linked event-executors job measured the current metric,
summed uncompressed Docker history layers, at 43,481,068 bytes (41.47 MiB).
Against the 112 MiB / 117,440,512-byte ceiling, that run had 73,959,444 bytes
(70.53 MiB) of headroom. This is evidence for that source head, not a size claim
for the current PR head.

The baseline used the classic Docker image store. The gate now sums exact
`docker image history --human=false --format '{{.Size}}'` layer bytes after
unpacking the image, preserving the uncompressed-layer ceiling on both stores.
With the containerd store, inspect `Size` includes compressed blobs **plus**
unpacked snapshots and is logged separately, not compared to that ceiling.
See [Docker's store documentation](https://docs.docker.com/engine/storage/containerd/)
and [Moby's layer-history implementation](https://github.com/moby/moby/blob/master/daemon/containerd/image_history.go).
The 112 MiB image ceiling is unchanged. The binary budget was 15 MiB at the
2026-10-01 calibration and is now 18 MiB (2026-10-24 recalibration below);
the 2026-10-10 measurements record the tighter tripwire it replaced.

Measured on 2026-09-30 in [PR #78's hosted container job](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/36770739970/job/110076173793)
at source `307b50708ec42e8fc4744c1b804216a22a17625e`. The 2026-09-30
calibration allowed roughly 25% image growth and 40% binary growth, rounded up
to the next 8 MiB and 1 MiB respectively. Base-image/toolchain changes must be
remeasured and any future budget increase justified. The 2026-10-01 S4
self-role runtime measured 10,805,344 bytes (10.30 MiB), against the main
baseline of 10,377,112 bytes (9.90 MiB) at `ec49663`; growth was linked
runtime/handlers/REST plus previously-dead domain/store code, with no new
dependencies and the same minimal release profile (opt-level=z, lto, strip).
Applying that historical binary calibration: 10.30 * 1.4 = 14.42 -> 15 MiB.
Docker is not available in the controller workspace; offline fixture sizes are
not measurements.

On 2026-10-10, the main binary measured 15,712,080 bytes (14.98 MiB), leaving
about 16 KiB under the 15 MiB guard. The RSVP bound-admissions slice (PR #709)
measured 15,734,960 bytes (15.01 MiB) with no new dependencies; the x1.4 rule
would yield 22 MiB, but the budget was set to 16 MiB instead to retain a useful
tripwire without wedging ordinary code changes. A later event-executors head
measured 15,770,928 bytes (15.04 MiB) in the [container smoke job](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/38062362788/job/114254309361),
42,288 bytes above the former ceiling. The internal-actions families (event
executors plus channel-moderation union) measured 15,741,344 bytes (15.01 MiB).
The then-current 16 MiB working tripwire left 1,006,288 bytes (0.96 MiB, 6.4%) above the
event-executors measurement. No new dependencies or release-profile changes;
revisit if the binary approaches the limit. The 112 MiB image ceiling is
unchanged.

On 2026-10-17 the 16 MiB ceiling was reached: main at `f2baa19` measured
16,776,096 bytes (1,120 bytes of headroom), and the stream delivered-dedup
expiry slice (PR #1079, `ac8fd66`) added 1,280 bytes with no new dependencies,
measuring 16,777,376 bytes — 160 bytes over the ceiling on hosted CI and
failing the check workflow on main twice, including a re-run of the failed
jobs. The release profile is already minimal (opt-level=z, lto, strip), so
the binary ceiling moves to 17 MiB; revisit if the binary approaches it. The
112 MiB image ceiling is unchanged (42.43 MiB measured on the same builds).

On 2026-10-17, the landed slices since the 16 MiB bump consumed its headroom:
main at `89c859b1` measured 16,777,376 bytes (16.00 MiB), 160 bytes over the
ceiling, so the gate trips on main itself. The PR-1093 merge_group against
the same base measured the identical 16,777,376 bytes in [its container smoke
job](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/39061090833),
confirming the trip is main-red, not PR-introduced. The
moderation-enforcement-outcomes
head measured 16,778,152 bytes (16.00 MiB), 936 bytes over, with no new
dependencies and the same minimal release profile. The budget is therefore
rebumped to 17 MiB, leaving about 1 MiB of headroom above the measured heads;
revisit if the binary approaches it. The 112 MiB image ceiling is unchanged.

On 2026-10-24 the 17 MiB ceiling was reached on main itself: main at
`cc07f31436` measured 17,845,208 bytes (17.02 MiB), 19,416 bytes over the
ceiling, in [its container smoke
job](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/40061835103/job/120785881426).
The staff game-mix slice head (`6a412c6a86`) measured 17,862,000 bytes
(17.03 MiB), 36,208 over, in [its container smoke
job](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/40040240318/job/120711266282),
with no new dependencies and the same minimal release profile (opt-level=z,
lto, strip). The budget is therefore rebumped to 18 MiB, leaving about 1 MiB
of headroom above the measured heads; revisit if the binary approaches it.
The 112 MiB image ceiling is unchanged (43.46 MiB measured on this head).

This branch's automation-only head separately measured 15,769,872 bytes
(15.04 MiB) before the event-executor merge. This PR raises the binary
ceiling to 17 MiB; the combined exact-head CI measurement is still
required.

### Docker history image measurement and immutable-ID pinning

Docker 29.8.1 with the containerd image store reports packed content plus
unpacked snapshot usage in inspect `Size`; that is not the uncompressed-layer
budget metric. See the exact-version
[Moby inspect implementation](https://github.com/moby/moby/blob/docker-v29.8.1/daemon/containerd/image_inspect.go),
[size accounting](https://github.com/moby/moby/blob/docker-v29.8.1/daemon/containerd/image_list.go),
and [Docker's containerd storage documentation](https://docs.docker.com/engine/storage/containerd/).
The original runner's storage backend was not recorded, so its historical
number is not proof that a current image fits the limit.

The gate reuses merged main [PR #145](https://github.com/TogetherWeOwn/two-bot-next/pull/145)'s
metric: **summed uncompressed Docker history layer bytes**, from
`docker history --no-trunc --human=false --format '{{.Size}}' IMAGE_ID`.
This is Docker history accounting, not unique tar-export bytes,
merged-filesystem size or compressed registry transfer size. The tag is
inspected once and its `Id` is used for history, binary measurement, the
runtime container and the no-server healthcheck probe. The storage-driver
inspect `Size` is printed for diagnosis only, never used as a fallback.

Each history record must contain a nonempty, nonnegative integer byte count.
Missing output, malformed/blank/negative records and an all-zero sum fail
before any containers are created. Zero-size metadata layers are accepted
alongside positive layers. History uses the existing 30-second Docker
subprocess timeout; a timeout or nonzero exit aborts measurement without
falling back to inspect `Size`. Offline fixtures cover this validation,
separate history/daemon sizes, immutable-ID use and subprocess failures.

The earlier **52/52 offline fixtures for the archive-parser implementation
are historical and superseded**, not current-head validation. That parser
failed real CI with `missing image config` in
[run 36829502178, job 110273465190](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/36829502178/job/110273465190).
The unused streamed-archive helper and its archive-specific builders/tests
have been removed in favor of main's history metric; the smoke/runtime
contract fixtures remain.

The 112 MiB image ceiling is unchanged; the release-binary ceiling is 18 MiB
(see the current calibration above). CI on this PR's exact head must still
record the corrected image size and pass the runtime contract and both
one-byte-budget negative checks; fixture success alone cannot establish
compliance. No current-head image-fit claim is made here. This measurement
repair does not change the Dockerfile, runtime base, CA assets or configured user.

The historical hosted parked-mode contract passed, including SIGTERM exit 0 in 0.095 s.
Manual log verification confirmed both deliberate one-byte-budget invocations
failed with the corresponding `exceeds size budget` error and that the CI
negative-test step passed. This exercises real measured artifacts, not mocks.

The smoke test starts the image with **no token, guild or database bindings**, a
256 MiB memory cap, and only a random loopback host port. It checks `/health` 200
with `status: ok`, `/readyz` 503 with process ready/gateway down, PID 1's non-root
UIDs, built-in `--healthcheck` exit 0 and Docker health status. A separate
no-network probe-only container must exit 1. SIGTERM must exit 0 within 10 s,
without OOM or a hidden SIGKILL fallback; the stopped container is inspected
before cleanup. This is a parked-mode contract, **not** evidence of real-guild
RSS or approval to change the B1 `basic` verdict above. Runtime RAM, image bytes
and executable bytes are different budgets.

Reproduce on an authorized Docker-capable development machine (not the
controller host):

```sh
docker buildx build --load --platform linux/amd64 -t two-bot:ci .
python3 scripts/container-smoke.py two-bot:ci
# Deliberate breakage: each invocation must fail with "exceeds size budget".
python3 scripts/container-smoke.py two-bot:ci --image-max-bytes 1
python3 scripts/container-smoke.py two-bot:ci --binary-max-bytes 1
```

The CI job exercises those two deliberately broken budgets against the real
image and fails if either violation is accepted. Offline Python fixtures also
cover missing binary, root runtime, unhealthy/false-ready endpoints, broken
healthcheck, OOM, shutdown exit/timeout failures, startup transport retries
(early-close loopback peer), redirect rejection (live `/readyz` 302 → `/other`
503 full-contract test), and named/capped auxiliary-container cleanup under
injected Docker-client timeouts:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_container_smoke.py' -v
```

Proposed branch protection: require **`container smoke`** alongside `check`,
`pr-lint` and `gitleaks` after the first green PR. This PR does not change
repository rules or production/staging deployments.

## Rust synthetic pipeline baseline — TOG-10886

Measured 2026-10-01 at source `445ca88f538c0a9b5913c1f1f53c12dd64501a52`.
The actual Twilight `Pipeline<GatewayFunnelBuffer>` and durable dispatch
transaction ran in an ephemeral Rust job container on
`[self-hosted, two-selfhosted]`, with mock REST and a job-private Postgres 18.6
service. No staging/production database or live Discord connection was used.
This is a **debug-profile synthetic pipeline**, not the entire bot or a
production sizing approval; the historical Node verdict above is unchanged.

| Metric | Standalone baseline | Same-head nightly repeat |
|---|---:|---:|
| Peak process RSS | 24.676 MiB | 24.082 MiB |
| Handler p50 | 18,675.202 µs | 2,857.018 µs |
| Handler p99 | 119,271.538 µs | 12,171.162 µs |
| Logical DB exchanges/event | 8.45 | 8.45 |
| Mock REST p50 / p99 | 3,945.614 / 48,439.975 µs | 1,025.072 / 1,393.941 µs |
| Paced replay time | 29.988 s | 29.971 s |
| Command time, including setup and verified teardown | 38.265 s | 30.929 s |

Sources: [standalone run 36821344440](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/36821344440)
and [nightly benchmark job](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/36821344778/job/110237375246).
Both uploaded `pipeline-benchmark.json` and the exact tested revision. The nightly
benchmark job passed; its separate broad sweep failed on database teardown
statement timeouts and rustdoc bare URLs, so this is not a claim that all nightly
jobs passed.

The original baseline retained the first successful standalone report verbatim
apart from added provenance, not an average, a synthetic envelope, or the faster
repeat. It is superseded by the journaled baseline below. Shared-runner latency
varied by about 6.5× at p50 and 9.8× at p99 between these runs; host contention
is a hypothesis, not a proven attribution. Treat the 25% comparison as a coarse
regression signal pending controlled-runner repeatability work, not a stable
latency SLA. Neither the tolerance nor baseline was raised to hide a failure.

Both reports prove 107 cached/durable members, 10 cached channels, 600 messages
plus 300 voice updates, 30 mock REST calls, 942 durable effects, checkpoint 1017,
and successful disposable DB drop. DB accounting is `(6705 completed SQL
statements + 900 unlogged BEGIN exchanges) / 900 measured events`; 900 observed
COMMITs independently validate the transaction count. It is not transport RTT
or prepared-statement handshake accounting. Handler nearest-rank percentiles
include handling, drain and awaited commit, but exclude pacing, JSON parsing
and the separately timed REST calls.

**Synthetic budget verdict: PASS** — both peaks are below the strict 200 MiB
target and both commands finish well under five minutes. No cgroup memory cap,
CPU budget, feature-runtime overhead, real-guild soak, or production resize is
proved by this result.

After building in the authorized CI job container, reproduce in under five
minutes with:

```sh
TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_pipeline_bench \
  timeout 240s target/debug/examples/pipeline_bench > pipeline-benchmark.json
python3 scripts/compare_pipeline_bench.py pipeline-benchmark.json
```

Compilation is separate. The existing strict test-database guard runs before
connection, creates/migrates a unique disposable database, and verifies its drop
before reporting success. See [the benchmark runbook](pipeline-benchmark.md)
for the build command, configurable workload, controller cache restrictions,
measurement definitions and non-required nightly integration.

### Journaled gateway pipeline baseline

The committed [baseline JSON](pipeline-benchmark-baseline.json) now retains the
first hosted success-path measurement at source
`8ea3f55f848472b6276162e4d1297bfec6fe2539`, measured 2026-10-21 in
[run 39637827461](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/39637827461).
This uses GitHub-hosted `ubuntu-latest`, an ephemeral Rust job container and
job-private Postgres 18.6, not the original self-hosted environment. All metrics
and evidence come from that one report; there is no per-metric selection.

- Peak RSS: **24.61328125 MiB**; handler p50/p99: **3,606.274 / 91,700.671 µs**.
- Logical DB exchanges/event: **13.685555555555556**, computed as
  `(11,417 completed statements + 900 BEGIN exchanges) / 900 measured events`.
- Mock REST p50/p99: **1,124.991 / 1,550.907 µs**; replay: **29.972988155 s**;
  total command including setup and verified teardown: **31.071836149 s**.
- Same workload: 107 members, 10 channels, 600 messages, 300 voice updates,
  30 REST calls, 900 commits and checkpoint 1017. Both migration chains applied;
  **942 durable events = 942 journal rows = 942 distinct journaled events**.
  The disposable database was dropped.

The measurement passed, but the workflow failed its old-baseline comparator:
`13.686 > 10.562 (25% tolerance)`. This is not a green workflow claim. Per-row
atomic best-effort journaling deliberately adds savepoint, journal insert,
watermark upsert and release statements. Retaining that design and refreshing
the measured baseline are reviewed together in [PR #1554](https://github.com/TogetherWeOwn/two-bot-next/pull/1554).
The 25% tolerance, strict 200 MiB RSS target and advisory p99 policy are unchanged.
The old 8.45 figure is historical, not the current journaled pipeline budget.
Latency across different runner environments is not a performance comparison;
this synthetic report still proves neither real-guild soak nor production sizing.

## Reproduce

Driver: `/tmp/tog9694/soak.mjs` (kept on the run host, not committed — it
points at an absolute checkout path). Scratch DB `tog9694_baseline` on
agent-testdb left intact for B2 cross-checks.

Gate policy (CTO decision, TOG-11786): the comparator fails on peak RSS,
SQL exchanges per event and handler p50 (25% tolerance). Handler p99 is still
measured and compared to the same limit, but a breach prints an `ADVISORY` line
and does not fail the job. Raising the p99 limit needs at least five controlled
repeats recorded in the baseline JSON plus independent QA acceptance.
