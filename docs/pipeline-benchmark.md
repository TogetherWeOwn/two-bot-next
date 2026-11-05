# Synthetic pipeline budget benchmark

`crates/discord/examples/pipeline_bench.rs` measures the same
`Pipeline<GatewayFunnelBuffer>` and `GatewaySessionStore::commit_dispatch` used
by the gateway, with the actual Twilight in-memory cache and disposable Postgres.
It is not a full-bot or real-guild soak, and cannot authorize a production resize.

## Reproduce

The non-required **pipeline benchmark** workflow runs on changes to its driver,
comparator or baseline, supports `workflow_dispatch` once merged, and is called
by the existing **nightly** workflow. It is not part of required `check`.
It follows the shared runner routing (hosted `ubuntu-latest` while the repo is public,
`[self-hosted, two-selfhosted]` when private; see `check.yml`) with an ephemeral Rust job container and
job-private Postgres service DNS, without publishing host ports; never staging/production.
The workflow uploads the JSON report and tested head SHA, including on failure.

Build in the CI job container first (compilation time is separate from replay time):

```sh
cargo build -p two-bot-discord --example pipeline_bench --locked
```

On the persistent controller, compiling commands must instead use the approved
`scripts/cargo_cache.py` wrapper from an isolated workspace. A missing/refused
pool is not permission for direct Cargo or an alternate cache. Ephemeral CI job
containers are the measurement path while the controller pool rollout is deferred.

With an authorized disposable `agent-testdb` container, pre-create an empty
`two_bot_test_pipeline_bench` bootstrap DB owned by passwordless `agent_test`.
The disposable server must preload `pg_stat_statements` and enable completed
statement JSON logging. The workflow supplies these **disposable-only** initdb
settings (Postgres 18):

```text
-c shared_preload_libraries=pg_stat_statements
-c logging_collector=on -c log_destination=jsonlog
-c log_min_duration_statement=0
-c log_rotation_age=0 -c log_rotation_size=0
-c autovacuum=off
```

Never enable these settings on a shared staging/production server for a benchmark.
The passwordless fixture principal needs permission to create the extension in
its own observer fixture and read that disposable server's log. Do not grant
production roles log access. Missing capabilities invalidate the measurement;
there is no client-only fallback. The bootstrap database is never migrated.

Then the measured command is:

```sh
TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_pipeline_bench \
  timeout 240s target/debug/examples/pipeline_bench > pipeline-benchmark.json
python3 scripts/compare_pipeline_bench.py pipeline-benchmark.json
```

The existing `two-bot-testsupport::TestDatabase` guard runs before connection:
exact test hostname/port/user, explicit empty password, test DB prefix, no URL
query/fragment or libpq environment overrides. No `DATABASE_URL` fallback exists.
The driver creates a uniquely named disposable database, applies both the cutover
and store migration chains (including the rollback journal), and verifies its drop
before emitting a success report. After replay, outside the measured query window,
it requires exactly one insert journal entry per durable event and reports both
journal-row and distinct journaled-event counts. Missing schema or incomplete
journal coverage fails rather than measuring the journal's best-effort failure
path. It never migrates/resets the bootstrap DB. On timeout/error the fixture still schedules cleanup; CI service teardown
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

## Independent durability and accounting guards

Before persistence, retain the expected idempotency keys of every staged funnel
event (seed included). After all serial dispatch commits have completed, close
the writer pool and wait for its database backends to exit. Reopen an independent
verification pool: its event key set must equal the expected set, every event
must have exactly one valid insert journal entry, and no event journal entry may
be orphaned. Comparing surviving event/journal counts alone cannot detect lost
work. One accepted gateway dispatch is **not** necessarily one new event row:
messages after funnel milestones can produce activity touches only. This guard
covers the benchmark's funnel events, not absent production feature-job queues.

`crates/cutover/tests/pipeline_durability.rs` drives the public dispatch seam in a
child process. A fixture-only AFTER INSERT trigger blocks the second row of a
three-row batch. The parent kills the writer process, observes backend exit,
requires the previous committed event/journal/checkpoint to survive with no
partial batch, restarts through a fresh pool, and replays the unacknowledged
batch. It verifies complete durability after drain and duplicate replay. Real
fixture corruption (missing row/journal or duplicate journal) must fail the same
oracle. The normal workspace integration lane and benchmark workflow run this
non-ignored test; it never kills a production database process. The benchmark
job builds it before applying the 120-second execution timeout.

```sh
cargo test -p two-bot-cutover --test pipeline_durability --locked
```

Use the bounded wrapper for controller compilation, as above. The existing full
suite, classifier/automod golden corpora, RSS gate and advisory p99 remain intact.
No benchmark pass substitutes for those checks.

### Transactions, completed exchanges and statistics profiles

All server observations come from a **second disposable database**, filtered to
the unique measured database. Observer reads cannot add transactions or SQLx
queries to the replay window. Flush setup statistics on each writer connection
before the first snapshot; close writer backends before the final snapshot.
`pg_stat_database` includes autovacuum/autoanalyze transactions, which are not
client dispatches. Initialize this fresh disposable server with `autovacuum=off`;
both snapshots reject an enabled setting. This excludes unrelated maintenance
from the fixture, not dispatch work from the metric. CI initially observed 935
commits for 900 dispatches with maintenance enabled; a disposable reproduction
with a one-second maintenance cadence observed 1135 commits. No guessed offset
or tolerance is subtracted. Never disable maintenance on staging/production.
The existing negative accounting tests still reject every extra transaction.

- `server_xact_commit` is the `pg_stat_database` commit delta minus **exactly two
  known observer transactions**. These are simple-query start/stop marker
  statements outside the measured client window, not prepared statements with
  separate Parse transactions. Both markers must occur exactly once in the
  server completion log. Require one workload commit per dispatch and zero
  rollbacks; report the observer count separately.
- `server_statement_calls` counts completed Execute/simple-query records between
  those markers in the server's JSON log. Parse/Bind records, seed, observer and
  verification are excluded. Errors, missing/duplicate markers, truncation,
  rotation, malformed records, unavailable logs or counter resets fail closed.
  Require exact equality to `sqlx_statements + begin_exchanges`, with no tolerance
  or empirical correction factor. The original metric formula is unchanged.
- `pg_stat_statements_profile` is a **distinct mechanism observation**, not a
  substitute for the completion count. On the tested Postgres 18.6/SQLx 0.9 path,
  repeated prepared utility statements are under-reported. A short 30-dispatch
  probe observed 439 top-level calls versus 540 logical exchanges. Equating
  those counters is invalid; neither a guessed offset nor SQLx utility counts
  supplies independent missing server evidence. The completed server log covers
  those utility exchanges independently. Reset/eviction invalidates the profile.
- `server_statement_profile` groups completed statements by PostgreSQL
  `hashtextextended(statement, 0)` fingerprint; bind-value log details never
  enter fingerprints or reports. Compare on the same PostgreSQL/toolchain
  environment. `event_statement_profile` reports observed dispatch counts and
  SQLx-plus-BEGIN exchanges by event family. Fingerprints identify mechanisms;
  they are not an isolation/security boundary.

Log reading is streamed in 128 KiB chunks, with a 64 MiB total budget, a 64 KiB
line cap inside the measured database/window and a 1024 distinct-statement cap.
Unrelated or earlier setup records are not subject to the measured line cap;
a partial JSON record remains bounded by the total budget until it can be scoped.
Because the logging collector is asynchronous, the reader follows appended bytes
for at most 10 seconds until the complete stop marker arrives, rather than treating
the first EOF as final. Regression tests cover oversized setup records spanning
chunks, rejection of oversized measured records, a delayed stop marker and awaited
first-fixture cleanup when observer setup fails. These database regressions live
in the non-ignored `testsupport/tests/pipeline_integrity.rs` integration target.
Normal integration, nightly and coverage sweeps provide a JSON collector, retain
maintenance defaults and enable completion logging only on the log-reader test's
single synthetic connection, not across unrelated suites.
Only fingerprints/counts are published. Full raw
logs may contain parameter values and remain inside the disposable server;
never upload them. Missing instrumentation is INVALID/NOT RUN, not a score.

For two new reports from independently executed same-workload development
replays (ordinary instrument validation, not qualified evaluation):

```sh
python3 scripts/compare_pipeline_bench.py after.json
python3 scripts/pipeline_bench_profile.py before.json after.json
```

The second command validates counter identities, complete durability evidence,
profile totals and metric arithmetic before reporting changed statement
fingerprints, per-family deltas and total exchange delta. It refuses the
historical baseline as a mechanism reference because that report predates the
independent counters. It creates no new target, tolerance or baseline ratchet.
The historic raw observation remains `(11417 + 900) / 900 = 13.685555555555556`,
not the old comparator failure limit of 10.562.

### Evaluator qualification is separate

These are executable correctness checks, not proof of author write exclusion,
held-out secrecy, distinct GitHub principals, confined read-only reviewers or
trusted result provenance. Same-UID worktrees, hashes, role prompts and native
review alone do not enforce those boundaries. This deliverable is limited to
ordinary, unqualified durability/accounting instrumentation. Its independent
review must confirm applicability to that scope before normal QA/CI/release
gates; shipment neither qualifies a panel nor reopens an optimization programme.
Optimization/climbing, blind panel use, held-out evaluation and trusted
model/provenance claims are excluded. Qualified use requires separately
authorized work proving enforced evaluator admission, containment, exact-head
binding and negative access tests. Never claim that this PR establishes those
missing controls or that development measurements are qualified scores.

## Ratchet

Baseline: `docs/pipeline-benchmark-baseline.json`, summarized in
[`b1-baseline.md`](b1-baseline.md). Comparator defaults to **25%** tolerance for
peak RSS, handler p50 and logical DB exchanges/event, plus an independent
strict **<200 MiB** RSS target (256 MiB lite ceiling cannot be overridden).
Handler p99 is **advisory**: it is printed as `ADVISORY` when beyond tolerance
but never fails the comparison, because it swings 6-232 ms on the shared runner
pool (CTO decision TOG-11786).
It fails closed on unavailable/nonfinite/negative metrics, reversed percentiles,
invalid limits, mismatched workloads and any gated regression beyond tolerance.
Exit codes: 0 PASS, 1 NEEDS WORK, 2 invalid measurement/input.

Offline comparator tests:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test*pipeline_bench*.py' -v
```

Driver fixture/percentile/bounds tests and database guard tests run in CI job containers:

```sh
cargo test -p two-bot-discord --example pipeline_bench --locked
cargo test -p two-bot-testsupport --lib --locked -- --test-threads=1
cargo test -p two-bot-testsupport --test pipeline_integrity --locked -- --test-threads=1
```

The journaled baseline uses [run 39637827461](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/39637827461)
at source `8ea3f55f848472b6276162e4d1297bfec6fe2539`: 942 durable events,
942 journal rows, 900 measured commits, and verified database drop. Measurement
passed; the workflow failed the old 8.45-exchanges/event baseline comparison.
The new report measures 13.685555555555556 exchanges/event and retains all
metrics from that one run. Per-row savepoint/insert/watermark/release deliberately
adds four statements per inserted row; it buys atomic best-effort journal evidence.
This baseline refresh is part of the same reviewed change, not a deferred nightly
repair. Tolerance and RSS limits are unchanged. See [the baseline record](b1-baseline.md#journaled-gateway-pipeline-baseline)
for the measured budget and its limitations.

Refresh a baseline only after inspecting the same-workload run evidence and
independent review; never ratchet it upward just to turn a regression green.
