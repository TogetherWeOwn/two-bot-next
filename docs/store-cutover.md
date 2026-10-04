# S6 store and cutover gates

## Implemented foundation

`two-bot-store` implements the core `FunnelStore` and `InviteSnapshotStore`
seams over sqlx/Postgres. Funnel writes arbitrate by the unique idempotency key,
then update the members projection in the same transaction. Duplicate delivery
cannot re-project a join; a failed projection rolls the event back. Message
milestones are first-write-wins, activity is monotonic, and a rejoin clears
`left_at` / `inactive_flagged_at`. Snowflakes remain decimal TEXT, timestamps
are TIMESTAMPTZ, and metadata stays ordered JSON TEXT for legacy row parity.

The stable migration directory is **`crates/store/migrations/`**. Bot versions
are 0001–0999 and unique across the workspace. The unpublished foundation uses
0400–0405, retaining the legacy SQL bytes under unused numbers; 0405 performs
conversion-aware normalization. Each file is checksum-locked in `migrations.lock`.
No staging/production deployment of the old proposed numbers is recorded; an
unexpected existing runtime ledger is a cutover gate, never rewritten here.
The migration ledger is
`_two_bot_migrations`; the cutover tools retain their own `_sqlx_migrations`.
Checksums, removed applied versions and out-of-range versions fail boot. Never
edit an applied migration: add a new version. `build.rs` tracks directory changes
so adding a migration refreshes the embedded runner. Tests exercise both orders
of application with the cutover tool chain; this is not proof that an arbitrary
existing production schema or historical migration ledger is compatible.
Normalization explicitly rejects legacy types with dependent views (including
materialized views) before conversion. Boot never drops/recreates consumers or
their grants. Such schemas need a separately authorized dependency-preserving
transition; this implementation does not provide or authorize that transition.

`sql/web_v1.sql` preserves the existing website contract. Application is atomic
and repeatable. `public` tables publish `web_v1`; a separate table schema publishes
`<schema>_web_v1`, matching the legacy test convention. CREATE OR REPLACE rejects
column removal, reordering and retyping. Null unpublished counts, the five rank
rows, milestone whitelist and bot/exclusion filtering remain unchanged. These
views are a SELECT contract, **not permission enforcement**: the deployment must
retain the existing website role's SELECT-only grants and lack of base-table
access. No roles, grants or credentials are created by this implementation.

The runtime pool is capped at **5**, with a **15-second statement timeout** and
10-second acquire timeout. Database initialization retains a **30-second client
deadline**. Main's DML-only gateway startup is preserved: initialization opens
the configured pool, checks schema/readiness prerequisites and hydrates durable
state without applying migrations or contract DDL. Provision both chains and the
contract through the separately authorized migrator path before boot; this slice
does not grant runtime credentials DDL permission. Configured initialization failure exits nonzero.
Incomplete token/DB/guild configuration keeps the gateway parked, as on main.
There is no ephemeral persistence fallback. `/health` stays
process-only; `/readyz` checks gateway state and a live DB ping (2-second bound).
HTTP readiness exposes no connection strings or database error text. Invite REST
failure/timeout/incomplete counters retain the durable baseline rather than
fabricating an empty listing.

The S5/S6 runtime stages each dispatch's funnel events, recency, bot flags and
invite snapshot changes in `GatewayFunnelBuffer`, hydrated from Postgres at boot.
One ordered blocking worker commits these effects **together with the gateway
sequence**, through `GatewaySessionStore`. Its client-side checkpoint deadline
covers acquire, all queries and COMMIT, including a silent acquired connection:
**at most 5 seconds**, capped at one quarter of HELLO's heartbeat interval.
Checkpoint failure never advances the durable replay cursor. No-op and unmapped
dispatches still checkpoint their sequences; duplicate sequences do not mutate
baselines. A restart resumes from the last committed cursor at its stored URL;
READY/RESUMED still discard open voice durations.

Shard reception remains independently polled. Queue capacity is **64**; overflow
stops reception and immediately sets gateway state to **Draining** (unready),
retains the received tail, and drains accepted work in order. An individual
handler has a **20-second watchdog** (REST is bounded to 10 seconds); the whole
drain, including tail enqueue and worker join, is bounded to **30 seconds**.
Successful checkpoint completion cannot restore readiness during drain.
Timeout/failure is fatal, not an in-process reconnect: a running `spawn_blocking`
handler cannot be forcibly cancelled. The essential-task supervisor exits the
process nonzero rather than waiting for Tokio shutdown or admitting a second
writer; uncommitted sequences replay after restart. On a deadline, full drain is
not guaranteed and must not be reported as complete. Observed bot classification
commits with departure projection, including MemberRemove first seen after a
restart, and human views exclude the bot. Receipt timestamps travel with queued
payloads; payload-provided timestamps take precedence.

The standalone `PgFunnelStore`/`PgInviteSnapshots` synchronous bridges remain
available to non-gateway callers on a multi-thread Tokio runtime. Their SQL
failures panic; async callers can use `PgFunnelStore::try_record` for an explicit
error result. The gateway does not call these unbounded standalone bridges.

Feature-owned moderation, automod, audit and settings migrations remain with
those slices; guild-settings hot reload is TOG-10096. Temporary voice rooms are
explicitly outside S6 (TOG-10091). This foundation does not claim those feature
ports or a live staging soak are complete.

## Local and CI verification

All database tests use **agent-testdb or ephemeral CI services only**, never
staging/production. Tests ignore `DATABASE_URL`, require `TEST_DATABASE_URL`,
validate the test host/identity before connecting, and create/drop only their
own uniquely named schemas. There is no credential fallback.

```sh
TEST_DATABASE_URL=postgresql://agent_test@agent-testdb:5432/postgres \
  cargo test -p two-bot-store --locked --test postgres --test membership_pg -- --ignored
cargo test -p two-bot -p two-bot-discord --locked
cargo clippy -p two-bot-store -p two-bot -p two-bot-discord --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

GitHub's required `check` runs the opt-in store tests against its Postgres service
container in addition to the normal workspace tests. Mock gateway payloads and
scripted invite counters are local doubles, not live guild evidence.

## Staging deployment checklist — not yet executed

Owner: DevOps & Reliability Engineer via the authorized deployment/broker path.
The author does not perform host actions, live Discord actions, or staging DB
probes. Tests continue to run on test containers only.

- [ ] Non-author review approved the exact merged head; required checks green;
      squash merge recorded. Any repush requires re-review on the same card.
- [ ] Confirm the authorized staging service, guild and existing secret bindings
      belong to this project. No production tokens, no credential substitution.
- [ ] Confirm the shared-Neon staging provisioning gate (TOG-9679) is satisfied.
      Stop if the expected credential or schema ownership fails; route to the
      Director of Engineering, never hunt for another credential.
- [ ] Preserve the current staging image/config and a provider-managed database
      restore point through the authorized operator path before boot migrations.
      Establish the approved schema/search_path; do not blindly replay against
      an arbitrary legacy migration ledger. 0405 refuses legacy conversion with
      dependent views; arrange a separately authorized dependency-preserving
      transition instead of dropping views, grants or editing checksums.
- [ ] Record the versioned image and deploy through the normal staging workflow.
      Confirm migration/contract startup success from sanitized runtime logs and
      `/readyz` database+gateway state. Failed startup is a failed gate, not a
      reason to bypass checks or edit the migration ledger.
- [ ] Obtain authorized staging runtime evidence that normal gateway arrivals
      record funnel events and projections, with duplicate delivery deduped.
      Use operator-provided sanitized runtime evidence; do not run engineer
      tests, fixtures, probes or verification queries against staging databases.
- [ ] Start the seven-day soak only after those gates pass. Record start/end UTC,
      versioned image, restart/error counts, persisted attribution and database
      readiness observations. Any data-loss/duplicate/contract regression stops
      the soak and returns to the same implementation card.
- [ ] Record staging acceptance and retain the rollback artifact before requesting
      any production cutover. A passing local fixture is not staging acceptance.

## Production cutover plan — authorization required, not executed

1. Assemble staging soak acceptance, exact reviewed SHA, required checks, schema
   compatibility and the authorized provider restore-point evidence. Record the
   old image/config and rollback owner. Get release authorization through the
   normal engineering/security chain; this document grants none.
2. Use an authorized operator/broker to stop the legacy consumer **before**
   admitting the replacement. Never run both writers for the same guild during
   cutover. Preserve its session/config and event ledger; do not reset data.
3. Apply only the reviewed schema transition through that operator path, deploy
   the versioned replacement image, then admit the single gateway consumer.
   Keep existing website SELECT-only grants and privacy boundaries unchanged.
4. Observe operational signals, not production DB test/probe queries. Trigger
   rollback on non-ready gateway/DB, migration/contract failure, lost or duplicate
   funnel writes, or any unapproved permission/exposure change.
5. Rollback: stop the replacement first; restore the prior image/config and
   resume the legacy consumer only after schema compatibility is confirmed.
   Do not roll back by deleting events, dropping the ledger, or editing checksums.
   If a schema restore is necessary, the authorized operator must reconcile new
   rows against the restore point before a provider-managed restore; never lose
   post-cutover events silently. If that cannot be demonstrated, keep the writer
   parked and escalate a bounded decision brief to the Director of Engineering.

No staging or production credentials, guild actions, deployment, restore point
or soak have been exercised by the S6 implementation tests.
