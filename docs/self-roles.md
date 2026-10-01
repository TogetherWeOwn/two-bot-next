# Self-role domain and storage seam

Source: frozen legacy `TogetherWeOwn/two-bot@d5d11793`,
`src/selfRoles/`, `src/discord/selfRoles.ts`, `src/store/selfRoleStore.ts`,
and migrations 0018–0023. This slice implements the domain and SQL store;
it does **not** enable live Discord dispatch.

## Modules and migration

- `crates/core/src/self_roles.rs`: catalogue parsing, live-role validation,
  permission/channel safety, hierarchy refusals, button/select/reaction plans,
  reply text and lease/event-order helpers.
- `crates/cutover/src/self_role_store.rs`: event deduplication/recovery,
  shared exclusive-panel leases, renewal, committed targets and fenced audits.
- `crates/cutover/migrations/0200_self_roles.sql`: final legacy schema,
  27 audit columns and 10 panel-claim columns, including JSON arrays as TEXT.
  It belongs to this slice's reserved 0200–0209 migration block and is embedded
  by the existing cutover migrator. Existing final-schema legacy tables are
  preserved. Upgrading a pre-0018–0023 legacy database is not this migration's
  purpose; the cutover must use the frozen legacy final schema.

## Executor contract

1. Respect the staging allowlist before registering any picker. An empty
   `TWO_SELF_ROLE_PANELS` disables the feature. `TWO_SELF_ROLE_DRY_RUN=1`
   must audit without emitting role mutations.
2. Resolve configured roles/channels and revalidate live permission masks and
   hierarchy before mutation. A configured role whose id is the guild id (the
   @everyone role) is rejected at catalogue validation when guild context is
   available and unconditionally at dispatch; @everyone stays in the snapshot
   as the channel permission baseline. Component input is untrusted. Reaction
   partials need fetches through the shared REST seam, not a private HTTP
   client.
3. Claim the interaction ID once. Reactions have no delivery ID: use a globally
   unique event ID per delivery and converge by planning against freshly fetched
   member state. Preserve its original timestamp and generated order on retries;
   the full event ID breaks same-millisecond ties across workers. SQL supersession
   uses the same byte ordering as Rust. Explicit add/remove duplicate plans are no-ops.
4. For an exclusive panel, acquire the shared guild/member/panel lane before
   fetching/planning. `Busy` is a retryable result; `Superseded` conveys **no
   ownership**. The executor supplies bounded retry/backoff, not the store.
5. Recover persisted desired/pre-mutation snapshots and `EventClaim.effects`
   instead of recalculating an old toggle or discarding checkpointed evidence.
   Renew both leases at the returned `renew_after_ms` interval; expired leases
   cannot renew. Store APIs do not accept caller timestamps: production uses
   PostgreSQL `clock_timestamp()` after pool acquisition and all relevant row
   locks, not transaction/statement-start time. Atomic settlement locks the panel
   then the audit before checking expiry. Only deterministic fixtures use the
   explicitly named `with_test_clock` constructor.
6. Use singular role mutations, removals before additions, retaining unrelated
   roles. Check ownership before **and after** each REST call. Storage fencing
   alone cannot cancel a remote request that outlives its lease.
7. Persist attempted/observed/compensated/unresolved effects truthfully.
   Attempted and compensated role IDs are cumulative historical evidence;
   checkpoint and settlement atomically union them with stored IDs. Observed
   added/removed and unresolved IDs are the latest snapshot and may be cleared
   after reconciliation. Recovery returns all eight arrays; it never infers
   historical effects from desired roles. On ambiguous REST failure, reconcile
   to the persisted pre-mutation state; an exclusive stale worker repairs only
   the last **committed** panel target.
8. Publish successful audit and panel target atomically using
   `finish_audit_and_set_panel_option`. A stale audit rolls back the target;
   a stale panel publishes nothing. Release retains chronology/commitment.
   Committed null target means empty selection; uncommitted null is unknown.

Settlement/effect writes intentionally follow the legacy token+generation
fence without an expiry check: late effect evidence may be recorded until
ownership transfers. When a newer exclusive-panel event supersedes an older
one, its rejection is terminal but the former worker may still record late
result/compensation evidence under its still-current token/generation via
`record_superseded_effects`; a transferred generation (new token) is refused,
and this authorizes no further REST work or panel-target publication. This is
not permission for another Discord mutation.
Unlike legacy's optional settlement claim, Rust requires an explicit claim;
it never looks up and borrows a different worker's token. The claim fencing
token is held in `Secret`, so derived `Debug` redacts it; the raw value is
exposed only at the SQL fencing comparisons. Malformed recovered
intent fails closed rather than silently becoming an empty selection.

## Verification

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core self_roles
python3 scripts/cargo_cache.py run -- test -p two-bot-core -p two-bot-cutover --lib
python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test self_role_store -- --ignored
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core -p two-bot-cutover --all-targets -- -D warnings
cargo fmt --all -- --check
```

The ignored integration test connects **only** to `agent-testdb:5432`, user
`agent_test`, empty password, database `agent_test`. It ignores inherited
`DATABASE_URL`, uses a unique schema, and drops only that generated schema.
Do not run it against production or staging. It proves concurrent single
winners, renewal/strict expiry, original-intent recovery, stale-token refusal,
chronology, committed empty targets, and atomic settlement rollback.

CI explicitly opts into the integration test in a job container with an
isolated `agent-testdb` Postgres service. No database port is published. The
required `check` job waits for that result and fails on failure, cancellation
or skip; the normal workspace test run alone still skips this opt-in test.

The S4 interaction router and REST executor are now merged in
[PR #57](https://github.com/TogetherWeOwn/two-bot-next/pull/57) and
[PR #63](https://github.com/TogetherWeOwn/two-bot-next/pull/63), respectively.
This PR remains domain/store-only as permitted by the slice contract.
[TOG-10292](/TOG/issues/TOG-10292) owns the bounded runtime follow-up and
still requires this slice to merge. It wires handlers, partial fetches and
compensation through those shared seams, never a feature-private dispatcher
or HTTP client.

A Common Changelog entry and Conventional Commit feature checkpoints provide
release notes. The release standard owned by
[TOG-10035](/TOG/issues/TOG-10035) is merged, and the flow published
[v0.2.0](https://github.com/TogetherWeOwn/two-bot-next/releases/tag/v0.2.0).
This slice's entry remains under Unreleased; it was not part of that release.
Do not manually bump Cargo versions, publish tags or create a release.
Non-author exact-SHA review and green required CI still gate squash merge.
No production guild, token, staging database or real Discord endpoint was
used to verify this seam.
