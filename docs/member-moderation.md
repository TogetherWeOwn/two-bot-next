# Member moderation domain and ledger

This slice implements `/ban`, `/tempban`, `/kick`, `/timeout`, `/warn`, and
scheduled unban processing. It does **not** register live handlers, instantiate
a Discord HTTP client, or start a scheduler. It stays independently testable
until the shared S4 interaction router and REST executor merge.

## Integration contract

- Feed current actor/target roles and permissions into `MemberExecution`.
  `MemberModerationService::execute` reuses `assert_moderation_allowed`; it
  validates before claiming or acting. Reasons are trimmed and capped at 512
  Unicode code points. Tempban bounds are 60 seconds–365 days; timeout bounds
  are 60 seconds–28 days.
- Implement `MemberDiscord` on the shared REST executor, not a second client.
  `DiscordCall` describes the plain-data mutation. Each call must abort after
  **5 seconds**, without automatic retries. Preserve legacy success handling:
  ban 200/204, kick and unban 200/204/404, timeout 200.
- Map definite Discord refusals to `Rejected`; timeouts, transport/5xx failures
  and rate limits remain uncertain. Only a definite refusal permits releasing
  a destructive claim or retrying an unban.
- Keep `TWO_MODERATION` off by default and restrict enabling it to staging.
  No production guild or token was used to verify this slice. The service is
  deliberately gate-independent: the shared router owns runtime gating.
- Construct **one moderation store per guild consumer**, cloning it for command
  and sweep paths. Store clones share local per-member FIFO queues. As in the
  legacy service, these queues are not cross-process Discord-effect locks.
  Do not run multiple independent consumers for the same guild.
- Drive `run_due_unbans` every `UNBAN_SWEEP_INTERVAL_SECONDS` (30), with at most
  25 claimed jobs per sweep. Surface failures; do not silently take over old
  `running` jobs. Disabling moderation must not abandon outstanding expiries.
- S5 owns authenticated audit-reason markers and the operational audit mirror.
  Until that integration, the Discord reason is the plain moderator reason.

## Durability

Migration `0110_moderation_member.sql` stays in `crates/cutover/migrations`,
the directory embedded by the S6 migration runner. It preserves legacy table
and column names for warnings, scheduled unbans, audits and idempotency.
`PgMemberModerationStore` is available under core's optional `db` feature;
framework-free tests use `MemMemberStore` and `MockMemberDiscord`.

Claims are atomic on `(guild_id, idempotency_key)` and bind the key to the
validated request content. A completed request replays its outcome. An
uncertain request remains `in_flight`, without timer takeover. SQL errors expose
only a classification/SQLSTATE, never bound parameters or database DETAIL.

Tempban writes a staged expiry **before** Discord, activates it after acceptance,
and cancels it after a definite refusal. Recovery takes the same member queue
and rechecks the staged state, so a live ban call cannot be mistaken for a crash.
A later expiry supersedes older staged/pending/running jobs; recovery processes
newest staged rows first. Overlapping sweeps atomically claim due jobs with
`FOR UPDATE SKIP LOCKED`. Claim tokens fence stale completion/requeue attempts.

Audit loss is logged after successful execution; it cannot cause a duplicate
mutation. A completion-write failure keeps the destructive claim uncertain.

## Verification

```sh
cargo test -p two-bot-core --locked
cargo clippy -p two-bot-core --all-targets --features db --locked -- -D warnings
MEMBER_TESTDB=agent-testdb cargo test -p two-bot-core --features db --locked \
  --test member_moderation_db -- --ignored
```

The database test accepts only `agent-testdb:5432` as `agent_test` with an empty
password, or the ephemeral CI Postgres service (`MEMBER_TESTDB=ci` inside GitHub
Actions). It never reads `DATABASE_URL`, Discord credentials, or inherited
credential fallbacks. Each run uses and cleans its own generated scratch schema.
The CI `moderation db` job executes that explicitly ignored integration test.
