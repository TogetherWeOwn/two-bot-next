# Gateway restart recovery (S5)

The single-guild, single-shard runner requires `DATABASE_URL` and `GUILD_ID` when a Discord token is configured. Without a token, health-only boot is unchanged. Database initialization or checkpoint failure leaves the gateway unready; it never silently falls back to an in-memory store. No bot token is stored in Postgres or printed in recovery logs.

## Checkpoint contract

Migration `0320_gateway_sessions.sql` is embedded in the existing `crates/cutover/migrations` runner (the S6 migration location). `gateway_sessions` is keyed by `guild_id, shard_id`; `session_id`, `seq`, `resume_url`, and `updated_at` describe the last **committed** dispatch.

1. Boot loads the checkpoint and restores only the funnel's message milestones and first-voice marker from the existing `events` table.
2. A checkpoint at most 15 minutes old is offered to Twilight with both `ConfigBuilder::session` and `resume_url`. The 15-minute limit is a conservative local policy, not a Discord session lifetime guarantee. Expired, future-dated, or incomplete state is removed before IDENTIFY.
3. The existing `Pipeline` handles each dispatch using `GatewayFunnelBuffer`, which stages its funnel rows and activity updates. The existing cutover members projection is reused. One sqlx transaction commits the rows, projection, activity and sequence. The core's legacy idempotency keys, including voice-channel suffixes, arbitrate row insertion.
4. Dispatches at or below the committed sequence in the same session are ignored **before** entering the pipeline. A new session can reset its sequence. Raw packet access lets unrecognized dispatch types advance a no-op checkpoint without losing ordering.
5. Opcode 9 with `d: false` clears durable state; Twilight reconnects and IDENTIFYs. With `d: true`, the checkpoint remains available for a RESUME retry. READY and RESUMED both mark readiness only after the dispatch commits.
6. Parse, compression or database errors stop this runner with readiness unavailable rather than advancing past uncommitted effects. Twilight retains ownership of network reconnect attempts and heartbeats. Restarting the process reconstructs the pipeline from committed storage.

A crash before commit rolls the batch and checkpoint back together; a crash after commit resumes beyond that batch. Every dispatch is checkpointed, so shutdown does not need a separate best-effort snapshot of an uncommitted websocket sequence. The tests abort and recreate both shard and pipeline to simulate a Container restart.

Open voice sessions are **never** restored. The existing pipeline drops them on READY and RESUMED (legacy `ShardReady`/`ShardResume`). This intentionally avoids outage-inflated durations. Twilight's channel/member cache is also cold after a process restart; this slice does not claim a fully restored cache or invite baseline. Facts, real leveling, REST side effects and invite fetching remain the existing S4/S6 seams; this transaction currently covers the S3 funnel, not arbitrary external side effects. Maintain one running shard owner per guild; overlapping independent deployments are not a supported recovery mechanism.

## Verification (test containers only)

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
TWO_GATEWAY_TEST_DATABASE_URL=postgresql://agent_test@agent-testdb:5432/agent_test \
  cargo test -p two-bot --locked gateway_tests -- --ignored --test-threads=1
```

The four opt-in tests cover stored-sequence RESUME after a new pipeline/shard, an unreachable original endpoint proving use of the saved resume URL, no duplicate repeatable funnel row when replay uses a newly generated timestamp, invalid-session fallback to IDENTIFY, stale expiry, restored message ladder, monotonic sequence, session reset, and transaction rollback after a deliberately invalid row.

Tests never consult runtime `DATABASE_URL`. Their dedicated URL is restricted to `agent-testdb` or the CI loopback Postgres service, database/user `agent_test`. Each test migrates its own generated schema and deletes only that schema. CI runs these tests explicitly against its service container.

## Rollback

Rollback the binary to the prior approved image/commit; leave migration 0320 and the existing event rows intact. The old S3 binary ignores `gateway_sessions` and identifies fresh. It is still the old in-memory S3 funnel, not a durable-store alternative. Do not delete or rotate credentials, drop shared tables, or apply a down migration. Redeploying the S5 binary after the rollback will either discard a stale checkpoint or let Discord reject it and IDENTIFY. A deployment/rollback drill is not performed by these tests, and production rollout remains a separate gate.

## Sources

- [Discord gateway resume and invalid sessions](https://docs.discord.com/developers/events/gateway#resuming): save READY's session and resume URL plus the latest dispatch sequence; `d: false` invalid-session requires a fresh IDENTIFY.
- [Twilight 0.17.1 ConfigBuilder](https://docs.rs/twilight-gateway/0.17.1/twilight_gateway/struct.ConfigBuilder.html#method.session): a saved session attempts RESUME; `resume_url` must accompany it.
- [Twilight 0.17.1 Shard](https://docs.rs/twilight-gateway/0.17.1/twilight_gateway/struct.Shard.html): raw stream messages drive transport state. Pinned source `src/shard.rs` handles opcode 9 through RESUME/NORMAL close, clearing session and URL for the latter.
- [sqlx 0.9 transactions](https://docs.rs/sqlx/0.9.0/sqlx/struct.Transaction.html): execute via the underlying connection and commit only after all effects succeed; dropping an unfinished transaction rolls it back.
