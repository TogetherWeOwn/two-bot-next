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
the $5.30/mo lite→basic step is not worth that risk. **Ship `basic`.** Revisit
`lite` only after the B2 staging-guild soak measures a real 107-member cache.

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
the 1/16-vCPU lite slice). B2's 7-day staging soak should replace this estimate
with a counted number.

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
this repository's Dockerfile on hosted `linux/amd64`, loads it into local Docker,
and uses BuildKit's `gha` cache. It never pushes an image or receives deployment
credentials. It also runs on `main` and workflow dispatch (including release
check dispatches). The existing required `check` job is unchanged.

`scripts/container-smoke.py` prints both sizes in bytes and MiB to the log and
job summary and fails above these calibrated ceilings:

| Artifact | Definition | Measured | Maximum | Headroom |
|---|---|---|---|---|
| Runtime image | Docker image inspect `Size` (uncompressed layers, not registry transfer size) | 87.19 MiB / 91,429,497 bytes | 112 MiB / 117,440,512 bytes | 24.81 MiB / 28.4% |
| Release binary | `stat` of `/home/two-bot/two-bot` in the final image | 7.01 MiB / 7,346,736 bytes | 10 MiB / 10,485,760 bytes | 2.99 MiB / 42.7% |

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
justify any future budget increase. Docker is not available in the controller
workspace; offline fixture sizes are not measurements.

The hosted parked-mode contract passed, including SIGTERM exit 0 in 0.095 s.
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

## Reproduce

Driver: `/tmp/tog9694/soak.mjs` (kept on the run host, not committed — it
points at an absolute checkout path). Scratch DB `tog9694_baseline` on
agent-testdb left intact for B2 cross-checks.
