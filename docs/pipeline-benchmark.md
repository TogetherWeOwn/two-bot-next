# Synthetic pipeline budget benchmark

`crates/discord/examples/pipeline_bench.rs` measures the same
`Pipeline<GatewayFunnelBuffer>` and `GatewaySessionStore::commit_dispatch` used
by the gateway, with the actual Twilight in-memory cache and disposable Postgres.
It is not a full-bot or real-guild soak, and cannot authorize a production resize.

## Reproduce

The non-required **pipeline benchmark** workflow runs on changes to its driver,
comparator or baseline, and supports `workflow_dispatch` once merged. No nightly
workflow currently exists; do not add this budget comparison to required `check`.
It uses an ephemeral hosted Postgres service, never staging/production.
The workflow uploads the JSON report and tested head SHA, including on failure.

Build on hosted CI first (compilation time is separate from replay time):

```sh
cargo build -p two-bot-discord --example pipeline_bench --locked
```

On the persistent controller, compiling commands must instead use the approved
`scripts/cargo_cache.py` wrapper from an isolated workspace. A missing/refused
pool is not permission for direct Cargo or an alternate cache. Hosted CI is the
measurement execution path while the controller pool rollout is deferred.

With an authorized disposable `agent-testdb` container, pre-create an empty
`two_bot_test_pipeline_bench` bootstrap DB owned by passwordless `agent_test`.
Then the measured command is:

```sh
TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_pipeline_bench \
  timeout 240s target/debug/examples/pipeline_bench > pipeline-benchmark.json
python3 scripts/compare_pipeline_bench.py pipeline-benchmark.json
```

The existing `two-bot-testsupport::TestDatabase` guard runs before connection:
exact test hostname/port/user, explicit empty password, test DB prefix, no URL
query/fragment or libpq environment overrides. No `DATABASE_URL` fallback exists.
The driver creates and migrates a uniquely named disposable database and verifies
its drop before emitting a success report. It never migrates/resets the bootstrap
DB. On timeout/error the fixture still schedules cleanup; hosted service teardown
is the final containment. No connection credentials are created or substituted.

Default parameters (all integer-valued) are equivalent to:

```sh
target/debug/examples/pipeline_bench \
  --members 107 --channels 10 --duration-secs 30 \
  --messages-per-sec 20 --voice-per-sec 10 --rest-every 30
```

Seed 107 joined members (and their durable join/gate effects) and 10 cached
channels, then replay 600 messages and 300 voice-state changes over a paced
30-second schedule. Voice membership alternates connect/disconnect per member.
One loopback mock REST reply per 30 events exercises the real `ActionExecutor`
with a non-secret fixture token; no Discord connection is made. Setup/seed is
outside handler percentiles, but inside process peak RSS and command elapsed time.
Parameters are capped at 10000 members, 1000 channels, 120 scheduled seconds,
1000/s per event family, 100000 events, and 1000 mock requests. Execution times out
at 180 seconds, leaving teardown headroom inside the 240-second command cap.
A larger profile needs its own baseline; unlike workloads are never compared.

## Measurement definitions

- **Peak RSS (MiB):** Linux process-lifetime `VmHWM` from `/proc/self/status`,
  after replay and row verification. Includes driver, mock server, SQLx,
  migrations, cache and latency samples. No cgroup cap is claimed.
- **Handler p50/p99 (microseconds):** exact nearest-rank quantiles of
  `handle_at` + buffer drain + awaited dispatch transaction. Includes cache,
  lock/read/write/projection/checkpoint/commit, excludes synthetic JSON parsing,
  schedule sleep and REST fixture calls. All 900 load events contribute.
- **DB round-trips/event:** logical completed SQL exchanges, not packet-level
  transport RTTs. Count SQLx 0.9 `sqlx::query` completion diagnostics during replay
  plus one `BEGIN` per successful dispatch. SQLx logs `COMMIT` but queues `BEGIN`
  directly; the driver verifies exactly one observed COMMIT per measured event.
  Prepared-statement describe/parse, connection handshake and transport packets
  are not counted. Seed, migrations, row-verification and teardown queries are
  excluded. Reported numerator, BEGIN count and denominator are inspectable.
- **Mock REST p50/p99:** separate outbound fixture-call latency. These replies
  are scripted benchmark actions, not new production pipeline side effects.
  The mock retains bounded requests; its memory is part of reported RSS.
- **Replay/command seconds:** schedule+processing time; total command includes
  creation/migrations/seeding and verified teardown. Compilation is not included.
- **Evidence:** durable event/member rows, cached population, final checkpoint,
  measured event mix, observed COMMITs, mock request count and verified DB drop.
  Missing diagnostics, empty effects, unexpected duplicates or failed teardown
  fail the command rather than producing plausible numbers.

The debug profile is deliberate: avoid long LTO builds and get a conservative
instrumented development baseline. Do not compare it to release/production or
infer whole-bot RSS. Leveling/facts hooks, command runtimes/background jobs,
gateway/TLS reconnect buffers, guild-role/chunk payloads and multiple guilds
are not modeled. Member/channel cache population is asserted, not inferred.

## Ratchet

Baseline: `docs/pipeline-benchmark-baseline.json`, summarized in
[`b1-baseline.md`](b1-baseline.md). Comparator defaults to **25%** tolerance for
peak RSS, handler p50/p99 and logical DB exchanges/event, plus an independent
strict **<200 MiB** RSS target (256 MiB lite ceiling cannot be overridden).
It fails closed on unavailable/nonfinite/negative metrics, reversed percentiles,
invalid limits, mismatched workloads and any regression beyond tolerance.
Exit codes: 0 PASS, 1 NEEDS WORK, 2 invalid measurement/input.

Offline comparator tests:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_compare_pipeline_bench.py' -v
```

Driver fixture/percentile/bounds tests and database guard tests run on hosted CI:

```sh
cargo test -p two-bot-discord --example pipeline_bench --locked
cargo test -p two-bot-testsupport --lib --locked
```

Refresh a baseline only after inspecting the same-workload run evidence and
independent review; never ratchet it upward just to turn a regression green.
