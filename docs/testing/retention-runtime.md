# Retention runtime acceptance contract

This acceptance contract began as a tests-first handoff extending the merged retention stores in [PR #923](https://github.com/TogetherWeOwn/two-bot-next/pull/923). The runtime implementation now includes the [gateway queue/cancel hooks](../../crates/bot/src/gateway.rs), the [`retention_jobs` scheduler adapter](../../crates/bot/src/retention_jobs.rs), and [`TWO_RETENTION_MODE` Worker forwarding](../../wrangler/src/container-env.ts); a public deletion endpoint and deployment changes remain out of scope.

## Runtime adapter

The production `retention_jobs` module provides two crate-visible functions and the configuration key used by boot:

```rust,ignore
const MODE_SETTING: &str = "TWO_RETENTION_MODE";

fn mode_from_setting(
    value: Option<&str>,
) -> Result<Option<two_bot_cutover::retention::RetentionMode>, jobs::ErrorClass>;

fn job(
    pool: sqlx::PgPool,
    mode: two_bot_cutover::retention::RetentionMode,
) -> jobs::Job;
```

The adapter parses `TWO_RETENTION_MODE` with the following exact, case-sensitive values. The boot fixture uses the adapter's `MODE_SETTING` constant rather than duplicating a production configuration key in test code. The non-secret runtime binding is classified in the existing Worker's `FORWARDED_FLAGS` allowlist, not as a test-only flag; the environment-drift guard remains in place.

| Setting | Result |
|---|---|
| Missing | Active dry run; no destructive work |
| `dry-run` | Active dry run |
| `execute` | Execute against the configured database |
| `off` | Parked; no attempt |
| Anything else (including blank, whitespace and uppercase) | Configuration refusal; parked; no attempt |
| Missing database, regardless of default mode | Parked; no attempt |

Register the DB-only job through `website_jobs::serve`, even when the Discord gateway is unconfigured. Expose a `retention` status entry, including when parked. Use the existing `serve_jobs` supervisor and its shared shutdown: do not add a detached timer. The test permits a cadence from 60 seconds through one hour, startup jitter at most five seconds, and a positive timeout no greater than cadence. Retain the supervisor's existing cancellation-and-join semantics on shutdown; this contract does not demand that shutdown wait indefinitely for SQL.

The real action must call the existing cutover retention operation. Every successful report has one structured `retention_run` event with `mode` and all `RetentionReport` counters, including `schema_gap_deferrals`. A partial `Ok(report)` with deferred/overdue deletions or failed guild purges must be visible at WARN, not silently reduced to scheduler success. Schema-coverage gaps defer as `Ok` (gap-deferred deletions in `schema_gap_deferrals` with the gap list in the warn log, queued guilds kept in `guild_purges_failed`) and retry after the plan is repaired; only SQL/connection failures return `ErrorClass::Database` to the supervisor. The tests accept the existing report field names; they do not invent a second erasure algorithm.

## Gateway contract

Use the actual serial dispatch/checkpoint path. Preserve `GuildDelete.unavailable: Option<bool>`: absent and false queue a purge, true never queues. All three still remove the live voice actor without deleting durable room rows or performing Discord writes. An available `GuildCreate` cancels the purge before its checkpoint; an unavailable create is not a rejoin.

A failed queue/cancel write must not acknowledge that dispatch or a later dispatch. Restart must resume from the last committed sequence and replay safely. A checkpoint failure after a lifecycle write may leave an idempotent queue mutation; the tests deliberately do not prescribe a new multi-table transaction API. A replay of an already committed delete must not undo a later available rejoin.

## Acceptance matrix

| Tests (all binary unit-test targets) | Observable assertion | Limit |
|---|---|---|
| `missing_configuration_is_dry_run_and_only_exact_execute_opts_in` | Missing/dry/execute/off plus seven invalid parser cases | Pure parser, supplemented by real boot below |
| `retention_cadence_retries_errors_only_at_the_next_bounded_opportunity` | Paused clock; no immediate error retry; next opportunity succeeds; no post-shutdown starts | Replaces only the real job's action |
| `retention_busy_ticks_never_overlap_or_catch_up_after_release` | One in-flight attempt; no busy-tick replay; shutdown drops and joins active work | Stretches test timeout after checking production bounds |
| `retention_timeout_cancels_then_retries_without_detached_work` | Timeout drops the first attempt; later bounded opportunity succeeds | Paused clock, injected pending action |
| `retention_already_stopped_owner_never_starts_work` | Pre-signalled shutdown never invokes an action | Existing supervisor ownership path |
| `dry_run_reports_candidates_and_preserves_raw_queue_requests_and_receipts` | Real supervised SQL action reports candidates; exact before/after JSON rows match | Fixed reviewed S1 tables, not every database table |
| `runtime_surfaces_deferred_overdue_and_completes_after_replay_fence_settles` | Two WARN reports, pending request/no receipt retained; settled replay fence then completes with receipt | Real cutover schema and replay guard |
| `runtime_surfaces_failed_guild_purge_and_retries_the_queued_work` | Injected delete failure remains queued; WARN counters; next real action succeeds after fixture trigger removal | Owned disposable fixture only |
| `runtime_schema_gap_defers_deletions_and_guild_purges_and_retries_after_repair` | Uncovered identity table defers the gapped deletion (counted in `schema_gap_deferrals`) and every queued guild purge (kept queued in `guild_purges_failed`) while time-window deletes commit; repair permits receipt and purge | Does not bypass schema coverage guard |
| `boot_registers_safe_modes_and_parks_invalid_disabled_or_missing_database` and child `isolated_boot_probe` | Six isolated processes call real `website_jobs::serve`; readiness records invocation/parking; execute mutates fixture; other modes preserve it; HTTP owner shuts down | No real Discord connection, deployment or staging assertion |
| `real_delete_flag_matrix_preserves_voice_cleanup_and_available_rejoin` | Three actual websocket delete variants, real voice actor cleanup, unavailable/available create, long-term rows survive subsequent retention | Fake Discord writes, real gateway and disposable store |
| `queued_delete_then_create_is_ordered_before_durable_checkpoint` | Database lock waiter proves queue write reached; checkpoint waits; ordered rejoin leaves no queue | Lock observation, no arbitrary timing sleep |
| `failed_queue_write_is_not_acknowledged_and_replays_after_restart` | INSERT refusal leaves old checkpoint; fresh runner RESUMEs and repairs; duplicate delete cannot requeue after rejoin | Synthetic websocket session |
| `failed_cancel_write_is_not_acknowledged_and_replays_after_restart` | DELETE refusal leaves old checkpoint/queue; replay cancels | Synthetic websocket session |
| `failed_checkpoint_after_delete_or_create_replays_without_losing_retention_work` | Checkpoint refusal for both lifecycle directions resumes and repairs from old sequence | Permits idempotent pre-checkpoint mutation |

The scheduler tests prove metadata plus the real supervision lifecycle. The database tests retain the actual adapter action, and the boot test invokes actual registration. These are complementary; passing injected-action tests alone is not acceptance of production wiring.

## Running

Use the toolchain and guarded disposable database setup in [CONTRIBUTING.md](../../CONTRIBUTING.md#database-tests). Never use application database bindings or staging/production databases. Each fixture uses `two_bot_testsupport::TestDatabase`, the existing cutover migrations, and an explicit verified close. No new ignored suite or custom CI database is needed; the normal binary-test lane supplies the bootstrap.

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- test -p two-bot --locked --bin two-bot retention_runtime_tests:: --no-run
python3 scripts/cargo_cache.py run -- test -p two-bot --locked --bin two-bot retention_runtime_tests:: --nocapture
python3 scripts/cargo_cache.py run -- test -p two-bot --locked --bin two-bot gateway_tests::voice::retention:: --nocapture
```

Database tests explicitly skip locally when `TWO_TEST_DATABASE_URL` is absent and fail if it is absent in GitHub Actions. A configured invalid/unreachable database fails, never skips. An all-green test summary without a configured bootstrap is **not** database acceptance; check for SKIP messages. The child probe is a helper that returns immediately unless launched by its parent test.

A refused Cargo cache lease is infrastructure refusal, not compiler RED and not a behavioral failure. Record that refusal separately from a test failure; do not bypass the wrapper. Run the relevant checks and same-head CI/review before merging.

## Handoff ownership and exclusions

The tests-first handoff added `crates/bot/src/retention_runtime_tests.rs`, its `database.rs` and `boot.rs` children, `crates/bot/src/gateway_tests/retention.rs`, and this document, with test registration in `main.rs` and `gateway_tests/voice.rs`. The implementation incorporated the test-author commits and added the gateway lifecycle hook, `retention_jobs`, registration in `website_jobs::serve`, and Worker forwarding. Previous S1 acceptance files remain unchanged; production code now invokes the retention core through these runtime paths.

Keep acceptance files immutable during implementation; report any demonstrated fixture defect to the test author instead of weakening an assertion. Event-store age TTL, a public user-deletion endpoint, production/staging rollout, and deployment configuration changes remain outside this slice.
