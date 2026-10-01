# Community jobs

## Website-contract runtime supervisor

The `two-bot` binary registers these named jobs in `bot::website_jobs`, using
`bot::jobs` as the reusable supervisor:

| Job | Cadence | Attempt timeout |
| --- | --- | --- |
| `counter` | 60 seconds | 45 seconds |
| `rank` | 10 minutes | 120 seconds |
| `scheduled_events` | 10 minutes | 120 seconds |

Each job gets one random startup offset in `[0, min(cadence, 5 seconds)]`.
The first attempt runs at that offset and subsequent deadlines keep the same
phase. A busy deadline is discarded, not queued: there is at most one active
attempt **per named job**, including REST reads and the database transaction.
Missed deadlines are skipped, so an overrun never creates a catch-up burst.
Timeout drops the job future; panics are isolated by Tokio task boundaries.
The release profile uses unwinding (not `panic=abort`) for the same guarantee
in the deployed binary. Jobs must remain asynchronous/cooperative; this is not
a preemption mechanism for blocking code.

Counter and rank also share one observation/publication lane: it is held from
raid-history and roster reads through the database commit because both ticks
write the denominator. A delayed rank tick cannot overwrite a newer counter
roster. Waiting for this lane consumes the attempt's timeout; cancellation
releases it. Scheduled events remain independent. Publication timestamps reuse
core `now_iso`, the fixed `YYYY-MM-DDTHH:mm:ss.sssZ` website contract.

The jobs park when `DISCORD_TOKEN`, `DATABASE_URL` or nonzero `GUILD_ID` is
missing. They share a paced, durably governed REST executor and lazily initialized
pools using the gateway's `DATABASE_URL` authority. The operator must provision
cutover migrations, `web_v1` and the reviewed role plan before runtime starts;
jobs never execute migration/view DDL using the DML-only runtime credential.
Admission refusal sends no HTTP. Initialization errors are retried on the next attempt, never
logged with a database URL. Guild members are fully paginated; rank-role names
come from the guild object's `roles` array. Domain/store semantics are unchanged:

- Counter and rank ticks publish nothing when historical raid windows cannot
  be grounded in imported funnel history. A deliberate skip is a successful
  attempt, not evidence that a fresh snapshot was written.
- Missing/ambiguous ladder roles or nonnested ranks refuse rank publication.
- Failed or malformed scheduled-event reads keep the previous mirror. Only a
  valid empty event array clears it.

`/readyz` retains its existing `components` array and adds an informational
`jobs` object keyed by the three names. Each entry carries `parked`, `running`,
`last_start`, `last_success` (Unix milliseconds), `last_error_class`, and
`consecutive_failures`. Successful attempts clear the error/streak. Error
classes are fixed identifiers, not SQL errors, REST bodies or panic payloads.
Job failures **never** change the HTTP readiness code; the gateway remains the
essential readiness gate. SIGTERM/SIGINT broadcasts cancellation before HTTP
drains; shutdown aborts and joins in-flight attempts and starts no more work.
An HTTP bind failure or gateway termination also signals cancellation. Gateway
termination allows HTTP up to five seconds to drain after cancellation, then
returns the restart error once the job supervisor's abort-and-join cleanup is
complete. Only the inner HTTP future has a drain deadline: expiry drops it but
never abandons the independent job join. SIGTERM/SIGINT uses the same bound;
a stalled drain returns a fixed timeout error. The HTTP-first path aborts and
joins the gateway. Cancellation remains sticky even before the HTTP future's
first poll, and a job awaiting the status lock cannot schedule a post-stop attempt.

Cadences cannot be overridden in a deployed binary. Test builds alone accept
positive `TWO_TEST_COUNTER_INTERVAL_MS`, `TWO_TEST_RANK_INTERVAL_MS`, and
`TWO_TEST_EVENTS_INTERVAL_MS` values. Paused-time regressions cover phase,
jitter bounds, overrun skips, timeouts, panic isolation and shutdown. The REST
adapter integration test uses the existing mock double and shared strict
`two-bot-testsupport` fixture, applying migrations in a unique disposable database
without migrating or resetting the bootstrap. Teardown is awaited. Run it
with an explicitly disposable database only. On the persistent controller, use
the bounded-cache wrapper (see [build-cache.md](build-cache.md)); refusal is not
permission to fall back to direct Cargo:

```sh
TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_tog10090 \
  python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot website_jobs::
python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot jobs::
python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot server::
python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot lifecycle_tests::
```

Without `TWO_TEST_DATABASE_URL`, the database test explicitly skips locally
(and refuses missing configuration in CI). It never consumes `DATABASE_URL`.
The existing CI unit/binary test step supplies the guarded service database.

## Presence probe, community scorecard, and inactivity flagging

This slice ports the three S5 community jobs as framework-free domain logic in
`two-bot-core` plus sqlx stores and migrations. The `bot::community_jobs`
module registers them on the job supervisor inside `bot::website_jobs::serve`
(TOG-10897), so the shipped binary drives them on their legacy cadences under
the env gates below.

## Modules

- `core::presence` — hourly probe decision (`decide_probe_cycle`), bot-floor
  re-list rule (`bot_floor_due`, 24 h), and the TOG-469 reopen trigger
  (`evaluate_trigger`: 45-peak threshold, 3 days in 14, 7-day minimum).
- `core::community` — classifier (`classify`, legacy precedence), weekly
  builder (`build_scorecard`), Monday 06:15 UTC schedule (`scorecard_tick`,
  60 s tick, exactly-once per Monday), env gates (`ScorecardGates`).
- `core::inactivity` — hourly quiet-member selection (`flag_inactive`,
  `TWO_INACTIVITY_DAYS ?? 14`). Read-only by construction: the outcome type
  carries no channel/message/DM field, so it cannot feed a send path.
- `core::presence_store` / `community_store` / `inactivity_store` — sqlx row
  moves behind the `db` feature, transliterated from the legacy queries.
- Migrations `0310_presence_probe.sql` / `0311_community_scorecard.sql` in
  `crates/cutover/migrations` (this card's reserved block 0310–0319).
  `community_facts` itself lives in 0160 (TOG-10083, host check-in facts) and
  is not recreated here.

## Integration contract

- Feed the REST guild-counts reading (`GET /guilds/{id}?with_counts=true`)
  into `decide_probe_cycle`; persist `Record`, drop `Skip`. A failed presence
  read writes nothing — not a null row, not a zero. Rescan the bot floor only
  when `bot_floor_due`; a failed listing keeps the presence row with a NULL
  floor. Drive the probe every `PRESENCE_PROBE_INTERVAL_MS` (1 h), unref'd,
  with one reading at startup.
- Drive the scorecard every `SCORECARD_TICK_INTERVAL_MS` (60 s); fire at most
  once per Monday via `scorecard_tick`. Before scoring, persist full-week
  stream coverage (`mark_stream_coverage` for all six streams); a mid-week
  start fails closed (`INGESTION_INCOMPLETE`, human numerators null).
- Drive the inactivity sweep every `INACTIVITY_SWEEP_INTERVAL_MS` (1 h) via
  `run_sweep`. Never DM, ping, or message from this outcome — any outbound
  contact needs CEO sign-off first.
- Implement fact writes on the gateway handlers through the `FactsSink` seam,
  classifying via `classify`. Keep `TWO_COMMUNITY_SCORECARD` off by default
  and `TWO_PRESENCE_PROBE` on (legacy default); restrict enabling to staging.
  No production guild or token was used to verify this slice.
- The presence series is never published: no `web_v1` view may read
  `presence_probe`. The only reader is an operator trend report over
  `read_series` + `evaluate_trigger`.
- Runtime registration (`bot::community_jobs::register`, invoked from
  `bot::website_jobs::serve`) resolves the env gates once at boot. A gated-off
  or misconfigured job logs `job_disabled` (warn on `invalid_config`), produces
  no supervised job, and is marked parked in the `/readyz` status map alongside
  the website jobs. The supervisor's status map therefore always lists all six
  job names.

## Verification

- Run `cargo test -p two-bot-core --features db --locked --lib <filter>`
  separately for `community::tests`, `presence::tests`, `inactivity::tests`,
  `funnel::tests`, `community_store`, `presence_store`, and `inactivity_store`,
  with `TWO_TEST_DATABASE_URL` pointing at agent-testdb. Cargo accepts one
  positional test filter per invocation.
- The store regressions include a Monday run replayed against a golden scorecard
  produced by the real legacy build (frozen two-bot @ `d5d11793`) and the
  inactivity never-messages invariant (exactly one `member_inactive` event row
  across two sweeps). Domain tests cover the week-boundary exactly-once tick,
  out-of-order message backfills, and missing versus explicit-null voice duration.
- Scorecard store tests apply the actual `crates/cutover/migrations` chain in
  isolated schemas, including 0160 and 0310–0311; no test-only fact table is used.
  `sqlx::migrate!` resolves its path relative to the crate's `Cargo.toml`:
  <https://docs.rs/sqlx/0.9.0/sqlx/macro.migrate.html>.
- Legacy table/column names kept verbatim; `CREATE TABLE / INDEX IF NOT EXISTS`
  throughout so S6 re-runs never fail on DDL.
