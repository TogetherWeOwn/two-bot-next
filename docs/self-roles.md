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
   hierarchy before mutation. Component input is untrusted. Reaction partials
   need fetches through the shared REST seam, not a private HTTP client.
3. Claim the interaction ID once. Reactions have no delivery ID: use a new
   event ID per delivery and converge by planning against freshly fetched
   member state. Explicit add/remove duplicate plans are no-ops.
4. For an exclusive panel, acquire the shared guild/member/panel lane before
   fetching/planning. `Busy` is a retryable result; `Superseded` conveys **no
   ownership**. The executor supplies bounded retry/backoff, not the store.
5. Recover persisted desired/pre-mutation snapshots instead of recalculating
   an old toggle. Renew both leases at the returned `renew_after_ms` interval;
   expired leases cannot renew. Supply fresh UTC time for every store call.
6. Use singular role mutations, removals before additions, retaining unrelated
   roles. Check ownership before **and after** each REST call. Storage fencing
   alone cannot cancel a remote request that outlives its lease.
7. Persist attempted/observed/compensated/unresolved effects truthfully. On
   ambiguous REST failure, reconcile to the persisted pre-mutation state; an
   exclusive stale worker repairs only the last **committed** panel target.
8. Publish successful audit and panel target atomically using
   `finish_audit_and_set_panel_option`. A stale audit rolls back the target;
   a stale panel publishes nothing. Release retains chronology/commitment.
   Committed null target means empty selection; uncommitted null is unknown.

Settlement/effect writes intentionally follow the legacy token+generation
fence without an expiry check: late effect evidence may be recorded until
ownership transfers. This is not permission for another Discord mutation.
Unlike legacy's optional settlement claim, Rust requires an explicit claim;
it never looks up and borrows a different worker's token. Malformed recovered
intent fails closed rather than silently becoming an empty selection.

## Verification

```sh
cargo test -p two-bot-core self_roles --locked
cargo test -p two-bot-core -p two-bot-cutover --lib --locked
cargo test -p two-bot-cutover --test self_role_store --locked -- --ignored
cargo clippy -p two-bot-core -p two-bot-cutover --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

The ignored integration test connects **only** to `agent-testdb:5432`, user
`agent_test`, empty password, database `agent_test`. It ignores inherited
`DATABASE_URL`, uses a unique schema, and drops only that generated schema.
Do not run it against production or staging. It proves concurrent single
winners, renewal/strict expiry, original-intent recovery, stale-token refusal,
chronology, committed empty targets, and atomic settlement rollback.

The router/REST adapter wiring, CI test-service execution, changelog/release
closeout and non-author exact-SHA review remain delivery steps. No production
guild, token, staging database or real Discord endpoint was used to verify
this seam.
