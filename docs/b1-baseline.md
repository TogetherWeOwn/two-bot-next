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

| Artifact | Historical definition | Measured | Maximum | Headroom |
|---|---|---|---|---|
| Runtime image | Docker image inspect `Size` (uncompressed layers, not registry transfer size) | 87.19 MiB / 91,429,497 bytes | 112 MiB / 117,440,512 bytes | 24.81 MiB / 28.4% |
| Release binary | `stat` of `/home/two-bot/two-bot` in the final image | 15.01 MiB / 15,739,344 bytes (2026-10-10; 10.30 MiB / 10,805,344 bytes at the 10-01 calibration) | 22 MiB / 23,068,672 bytes | 6.99 MiB / 31.8% |

The baseline used the classic Docker image store. The gate now sums exact
`docker image history --human=false --format '{{.Size}}'` layer bytes after
unpacking the image, preserving the uncompressed-layer ceiling on both stores.
With the containerd store, inspect `Size` includes compressed blobs **plus**
unpacked snapshots and is logged separately, not compared to that ceiling.
See [Docker's store documentation](https://docs.docker.com/engine/storage/containerd/)
and [Moby's layer-history implementation](https://github.com/moby/moby/blob/master/daemon/containerd/image_history.go).
Neither the 112 MiB image nor the 10 MiB binary budget is increased.

Measured on 2026-09-30 in [PR #78's hosted container job](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/36770739970/job/110076173793)
at source `307b50708ec42e8fc4744c1b804216a22a17625e`. Ceilings allow roughly
25% image growth rounded up to the next 8 MiB, and roughly 40% binary growth
rounded up to the next MiB. Base-image/toolchain changes must remeasure and
justify any future budget increase. Recalibrated 2026-10-01 for the S4 self-role
runtime (TOG-10292): PR head measured 10,805,344 bytes (10.30 MiB) on the
ephemeral runner vs main baseline 10,377,112 bytes (9.90 MiB) at `ec49663`;
growth is linked runtime/handlers/REST plus previously-dead domain/store code
with no new dependencies, release profile already minimal (opt-level=z, lto,
strip). Per calibration (measured * 1.4 rounded up to the next MiB):
10.30 * 1.4 = 14.42 -> 15 MiB. Docker is not available in the controller
workspace; offline fixture sizes are not measurements.

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

The **112 MiB image and 10 MiB binary ceilings are unchanged**. Real current-head
CI must still record the corrected image size and pass the runtime contract and
both one-byte-budget negative checks; fixture success alone cannot establish
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

The committed [baseline JSON](pipeline-benchmark-baseline.json) retains the first
successful standalone report verbatim apart from added provenance. It is not
an average, a synthetic envelope, or the faster repeat. Shared-runner latency
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

## Reproduce

Driver: `/tmp/tog9694/soak.mjs` (kept on the run host, not committed — it
points at an absolute checkout path). Scratch DB `tog9694_baseline` on
agent-testdb left intact for B2 cross-checks.

Gate policy (CTO decision, TOG-11786): the comparator fails on peak RSS,
SQL exchanges per event and handler p50 (25% tolerance). Handler p99 is still
measured and compared to the same limit, but a breach prints an `ADVISORY` line
and does not fail the job. Raising the p99 limit needs at least five controlled
repeats recorded in the baseline JSON plus independent QA acceptance.
