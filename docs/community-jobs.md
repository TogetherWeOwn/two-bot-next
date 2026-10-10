# Community jobs

## Website-contract runtime supervisor

The `two-bot` binary registers these named jobs in `bot::website_jobs`, using
`bot::jobs` as the reusable supervisor:

| Job | Cadence | Attempt timeout |
| --- | --- | --- |
| `counter` | 60 seconds | 45 seconds |
| `rank` | 10 minutes | 120 seconds |
| `scheduled_events` | 10 minutes | 120 seconds |
| `settings` | 15 seconds | 10 seconds |

`settings` is the `guild_settings` hot-reload poll (TOG-10898), registered by
`bot::settings_jobs` before the REST executor is built: it needs only
`DATABASE_URL`, so a bad `DISCORD_API_BASE` cannot park it. Each tick reads
the transactional revision plus row count; only a moved mark pays for the
consistent snapshot load, which is validated (no empty `guild_id`/`key`, no
duplicate pairs — refused whole with `settings_snapshot_rejected` and the
previous snapshot kept) and published through a `watch`-carried
`Arc<SettingsCache>` (`two_bot_core::settings::live_channel`). Feature
runtimes read `settings_jobs::live()` — `None` while the poll is parked; a
reader before the first publish sees the empty revision-0 cache and falls
through to the environment either way. Applied swaps log `settings_applied
{version, keys}` (key names only), `setting_changed` per hot key,
`settings_restart_required` per stored-but-cold key, and
`setting_ignored_not_applied` per env-only/unknown row. The published cache
contains only `HOT_WIRED` keys: cold and hot-but-unwired keys remain in the
writer's stored-state cache for diff/logging, never in `get()`, `snapshot()` or
`env_snapshot()` exposed to live consumers. `settings_applied.keys` lists only
hot-applied keys; restart-required keys have their own log event.

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
come from the guild object's `roles` array. Domain/store semantics are unchanged
except for the rank self-heal below, the one tick that writes to Discord:

- Counter and rank ticks publish nothing when historical raid windows cannot
  be grounded in imported funnel history. A deliberate skip is a successful
  attempt, not evidence that a fresh snapshot was written.
- Missing/ambiguous ladder roles refuse rank publication. Non-nested ranks
  self-heal when the live-identity fence permits the `rank_heal` capability:
  the tick grants the missing lower rungs (bounded, hierarchy-fenced, with an
  audit reason) and republishes; a refused identity keeps the old refusal.
- Failed or malformed scheduled-event reads keep the previous mirror. Only a
  valid empty event array clears it.

`/readyz` retains its existing `components` array and adds an informational
`jobs` object keyed by the four names above plus the three community-job names below. Each entry carries `parked`, `running`,
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
positive `TWO_TEST_COUNTER_INTERVAL_MS`, `TWO_TEST_RANK_INTERVAL_MS`,
`TWO_TEST_EVENTS_INTERVAL_MS`, and `TWO_TEST_SETTINGS_INTERVAL_MS` values. Paused-time regressions cover phase,
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
TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_tog10898 \
  python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot settings_jobs::
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

The live-identity capability fence (`activation.rs`) does not narrow these three
jobs, by decision rather than omission: none of them writes to Discord. The
presence probe only reads the guild and roster, and the scorecard and inactivity
sweep touch Postgres alone, so there is no `LiveCapability` to bind them to. The
scheduled-message ticker and the feed poller do post, and are fenced. The test
`community_jobs_have_no_discord_write_path` fails if a community job gains a
write verb; bind that job to a capability in `BootActivation` before it ships.

## Modules

- `core::presence` — hourly probe decision (`decide_probe_cycle`), bot-floor
  re-list rule (`bot_floor_due`, 24 h), and the TOG-469 reopen trigger
  (`evaluate_trigger`: 45-peak threshold, 3 days in 14, 7-day minimum).
- `core::community` — classifier (`classify`, legacy precedence), weekly
  builder (`build_scorecard`), Monday 06:15 UTC window (`scorecard_tick`), env
  gates (`ScorecardGates`). The runtime uses the bounded retry planner below
  rather than the older process-only once-per-Monday marker.
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
  with one reading at startup. A per-process overlap lease
  (`PRESENCE_PROBE_LEASE_MS`, 30 min) makes a concurrent trigger skip with
  `presence_probe_overlap_skipped` before any REST call; a holder older than
  the lease is presumed dead and taken over.
- Drive the scorecard every `SCORECARD_TICK_INTERVAL_MS` (60 s). The pure
  `bot::community_jobs::scorecard_retry::decide(state, now)` returns attempt,
  wait or skip: at most three reservations, five minutes apart, only Monday
  06:15–06:59 UTC. The adapter commits count/backoff before work and completes
  the week only after success. A cancelled/crashed attempt still spends a slot.
  `community_scorecard_attempts` (additive migration 0312) stores the per-guild,
  per-Monday budget in the same Postgres database as the output ledger; the
  former process-only marker could not satisfy a restart limit. A row lock
  serializes competing reservations. The five-minute backoff exceeds the
  supervisor's two-minute attempt timeout; the process lane also skips overlap.
  On retry, any existing output for the closed week is terminal, even if its
  watermark/classifier changed or the completion write was lost. A successfully
  persisted incomplete scorecard is terminal, not a transient failure.
  Before scoring, mark honest stream coverage for the captured streams only
  (`CAPTURED_STREAMS`, today `event_attended`, `message_created`,
  `rules_accepted` and `member_joined`) from capture start through
  the closed week end; a mid-week start fails closed (`INGESTION_INCOMPLETE`,
  human numerators null). A Monday boot cannot claim closed-week coverage:
  leave missing heartbeats missing rather than inserting an inverted interval.
  Apply the reviewed database-role plan after migration 0312; the object matrix
  includes the private scheduler table, with no website reader grant.
- Drive the inactivity sweep every `INACTIVITY_SWEEP_INTERVAL_MS` (1 h) via
  `run_sweep`. Never DM, ping, or message from this outcome — any outbound
  contact needs CEO sign-off first.
- Implement fact writes on the gateway handlers through the `FactsSink` seam,
  classifying via `classify`. The `message_created` writer is live
  (`DeferredCommunityFacts`, armed only when `TWO_COMMUNITY_SCORECARD=1`):
  gateway `MessageCreate` events land via `message_fact` + `record_fact`,
  keyed `discord-message:{message_id}` so duplicate delivery returns `false`;
  DMs never reach the sink (dropped in the pipeline) and bots, webhooks and
  staff automation are captured but never funnel-counted. The
  `rules_accepted` writer shares the same sink: gate-clearings buffer raw and
  drain through `rules_accepted_fact` + `record_fact` on the serial worker,
  keyed `rules-accepted:{guild}:{member}` so repeat clears return `false`; a
  failed drain only warns and the scorecard fails closed on the missing fact.
  The `member_joined` writer shares the same sink: gateway joins buffer raw
  with their invite attribution (`source` + `inviterId` metadata) and drain
  through `member_join_fact` + `record_fact` on the serial worker, keyed
  `member-join:{guild}:{actor}:{occurred_at}` so a redelivered burst returns
  `false`; bots are captured but never funnel-counted.
  Keep `TWO_COMMUNITY_SCORECARD` off by default and `TWO_PRESENCE_PROBE` on
  (legacy default); restrict enabling to staging. No production guild or
  token was used to verify this slice.
- The presence series is never published: no `web_v1` view may read
  `presence_probe`. The only reader is an operator trend report over
  `read_series` + `evaluate_trigger`.
- Runtime registration (`bot::community_jobs::register`, invoked from
  `bot::website_jobs::serve`) resolves the env gates once at boot. A gated-off
  or misconfigured job logs `job_disabled` (warn on `invalid_config`), produces
  no supervised job, and is marked parked in the `/readyz` status map alongside
  the website jobs and settings poll. The supervisor's status map therefore
  always lists all eleven job names (the six website/community jobs plus the
  audit-retry job, the scheduled-messages job, the self-role recovery job, the
  gated `feeds` job and the DB-only settings poll, each parked when its service
  is unregistered), even when announcements are off.

## Verification

- The six legacy retry cases are ported in `community_jobs_tests.rs`: transient
  coverage/scoring failures, exhaustion/new week, exact window boundaries,
  in-flight exclusion and supervisor stop. Additional tests cover reloaded crash
  reservations, database failures/restarts, concurrent claims, commit/completion
  reconciliation with a changed classifier and honest Monday-boot coverage.
  Run the smallest controller target through the bounded wrapper:
  `python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot community_jobs::`.
  Database cases require `TWO_TEST_DATABASE_URL` on agent-testdb/CI services;
  absent configuration skips locally and fails in CI. Existing hosted CI's
  unit/binary step runs them without changing the workflow.
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
