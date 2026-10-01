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

Late effect writes and the legacy `finish_audit` evidence/rejection seam follow
the token+generation fence without an expiry check: late evidence may be recorded
until ownership transfers. Runtime settlement uses `finish_owned_audit` or
`finish_audit_and_set_panel_option`: both require initialized intent, no pending
exchange, and a live event after lock waits. Atomic publication also requires a
live panel; either refusal rolls back the target and audit together. When a newer exclusive-panel event supersedes an older
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
The domain/store slice [PR #43](https://github.com/TogetherWeOwn/two-bot-next/pull/43)
is also merged. [TOG-10292](/TOG/issues/TOG-10292) owns the bounded runtime
follow-up. Its first checkpoint adds `executor::self_roles` on the existing
`ActionExecutor`, not a private dispatcher or HTTP client:

- `fetch_self_role_snapshot` force-fetches target member, bot member, guild roles
  and channels. Missing/duplicate identities, unknown member roles, invalid masks
  and partial policy fail closed. The domain validator consumes the live snapshot
  for hierarchy, deployment-mask drift and channel-overwrite checks.
- `fetch_self_role_message` resolves reaction partials and validates the returned
  message/channel identity. Member state still needs the authoritative fetch.
- `self_role_step` uses one singular PUT or DELETE, one bounded exchange, and the
  shared 110 ms pacing reservation. A caller-provided DB ownership check runs
  after pacing and again after the call. No automatic retry can escape those
  checks. A `RoleExchange` retains accepted/ambiguous effects even when the
  post-call check loses ownership or fails. Only Discord's documented 204 is
  accepted for a role mutation; other 2xx/3xx are uncertain.

The next checkpoint adds `crates/bot/src/self_role_runtime.rs` admission and
planning on the same executor:

- Admission claims the event before REST, then acquires the exclusive lane with
  a bounded 20-second wait. Duplicate and superseded inputs never fetch or plan.
  Scoped recovery refuses a changed member, panel, source, order or operation.
- Separate cancellation-safe renewal tasks keep both claims live during REST,
  pacing and database waits. A renewal failure latches a stop flag. Dropping the
  prepared operation stops both tasks; it does not pretend to cancel remote work.
- Migration `0201_self_role_intent_initialization.sql` adds one boolean to the
  existing audit table (28 columns after upgrade). Existing snapshots remain
  initialized. Runtime admission explicitly records an uninitialized intent;
  the first authoritative before/desired snapshot initializes exactly once under
  a live token/generation fence. An intentionally empty target is immutable, not
  a placeholder that a recovery worker may replan.
- Reaction partials fetch the configured message; all surfaces force-fetch member
  and policy after lane admission. A previously unknown lane may be seeded only
  from a fresh, unambiguous held selection, never from incoming intent.
- Recovery plans from immutable before/desired sets, retains all eight effect
  arrays, and separately computes remaining work against current roles. Changed
  catalogue/input intent fails closed; unrelated roles are not mutation targets.

The execution checkpoint adds durable, bounded role convergence, still **not**
registered with the gateway:

- `self_role_step_journaled` persists attempted/unresolved send intent inside the
  shared paced reservation, after a live fence, and checks ownership again after
  the database wait. Failed journaling prevents the send. Cancellation after
  journaling is unresolved intent, not proof that Discord received a request.
- Each execution step fetches and validates fresh member/hierarchy/permission
  state, removes before adding, and targets only configured panel roles. A 204
  records an observed exchange; definite rejection and ambiguous exchanges are
  distinguished. Late results can update evidence but authorize no more sends.
- Migration `0202_self_role_compensation_phase.sql` adds a monotonic rollback
  boolean (29 audit columns after upgrade). Failed exchange evidence and the
  rollback decision checkpoint atomically; neither a retry nor an applying
  checkpoint can reset it. Recovery restores the immutable before target, not
  the original desired target. Compensation retains attempted and confirmed
  restoration history while net observed deltas follow authoritative reads.
- `Execution::Applied`/`Compensated` are provisional convergence results, **not**
  final audits or user success. A transport timeout cannot prove a remote call
  has stopped. The settlement/repair checkpoint below consumes these results
  only after additional authoritative verification and live database fences.
- Isolated DB/mock regressions cover remove-then-add, partial rejection,
  an ambiguously applied addition, failed compensation and restart into rollback.
  Store coverage proves phase/evidence recovery and stale-generation refusal;
  REST coverage proves failed journaling and post-journal loss prevent sends.

The settlement/repair checkpoint adds these runtime seams, still without handlers:

- `settle` re-fetches authoritative policy/member state, requires convergence to
  the immutable target, and commits a successful audit and exclusive target
  atomically. A fully restored compensation is a rejected audit, not success.
  Nonexclusive settlement requires the same live event fence. Release failure
  cannot change an already committed outcome into a false rejection.
- `reconcile_stale` stops old keepers and acquires a **new** renewing maintenance
  lane without changing event chronology. It force-fetches and revalidates policy,
  removes before adding, and restores only the last committed option (including
  committed empty). Unknown targets and catalogue drift fail closed. Late effects
  retain terminal supersession and cannot publish an obsolete before/desired set.
- Migration `0203_self_role_pending_exchange.sql` adds `exchange_pending` (30
  audit columns after upgrade). Journaling sets it before send. Only a received
  response or a definite no-send path may clear that exchange's flag; a later
  acknowledged compensation cannot clear an earlier interrupted exchange.
  Recovery retains pending state, switches to rollback, and preserves unresolved
  evidence even when a read looks restored. Pending audits cannot settle or
  publish success. Generation transfer refuses old workers' flag/effect writes.
- Added regressions exercise late in-flight 204 after a newer worker commits,
  repair to both selected and empty targets, unknown-target refusal, interrupted
  exchange recovery, settlement of success/compensation, event-expiry rollback
  with a still-live panel, and REST timeout completion evidence.

The handler checkpoint adds an **injectable**, still boot-disabled service:

- `CommandRuntime` uses the existing shared self-role component outcome and
  reaction add/remove gateway hooks, not another router or HTTP client. Input
  validation requires the configured guild/channel/message and matching actual
  button/text-select type. Modal input, unknown options, duplicate selections,
  excess exclusive selections and bot identities fail closed. Empty selects are
  valid. Reaction-remove partials and deleted custom emoji names use the shared
  message/member/policy reads and custom emoji ID respectively.
- Component work defers ephemerally through `ActionExecutor` before admission.
  Failed acknowledgement performs no database admission or role mutation.
  Replies distinguish final settlement, simulation, duplicate and unresolved
  work; provisional execution or stale repair never produces a success reply.
- `settle_dry_run` writes a live-event-fenced `rejected` audit with code `dry_run`
  and immutable proposed intent, without role mutations or publishing a simulated
  panel target. Recovered attempted effects, compensation and pending exchanges
  cannot be relabeled as fresh simulations. Unresolved owners stop their keepers
  and release only their own fenced lane, retaining the processing audit.
- The service constructor requires a nonempty catalogue and its guild in an
  injected approved staging allowlist. This is not process-level rollout wiring:
  `CommandRuntime::from_env` still injects no service, and default router surface
  flags remain disabled. Approved staging configuration and boot construction
  are unfinished.
- Added unit input tests and isolated mock/Postgres orchestration regressions
  cover shared select replacement, duplicate delivery, dry-run, failed defer,
  disabled routing and recovered-mutation dry-run refusal. Reaction dispatch now
  respects the shared router gate; its fixture exercises partial add/remove and
  repeated explicit add/remove as no-ops through the actual gateway hooks. These
  are source coverage, not passed Rust acceptance.
- Expired processing audits can be discovered without retaining or redelivering
  gateway input. Discovery is scoped to configured guild/panel/message/source,
  ordered by expiry then a stable event-ID tie-breaker, and capped at 32 rows.
  It grants no authority or snapshot: the existing claim lock reloads immutable
  intent/effects and rotates the generation. A service pass considers at most
  32 rows across at most eight panels, four per panel, rotating its starting panel
  before awaiting work; renewed expiry supplies durable backoff. Initialized empty
  select targets remain meaningful, while interrupted uninitialized intent rejects
  without REST. Inconsistent pending or
  effect evidence stays processing even when initialization is absent.
- Added isolated discovery/race/limit and recovery-without-redelivery fixtures
  cover selected/empty targets, compensation, dry-run/pending refusal and
  uninitialized evidence preservation.
- `recovery_job` awaits a pass inside the existing fixed-phase supervisor, with
  a 30-second cadence, bounded startup jitter and 25-second attempt timeout.
  `serve_with_self_roles` accepts the same injected service Arc used for dispatch;
  no new scheduler, detached recovery task or HTTP client is created. Per-name
  status parking keeps unavailable website/community jobs parked even when only
  recovery is injected. A successful tick means the sweep completed, not that
  every pending exchange settled. Shutdown/timeout drops owners and their renewal
  keepers without clearing pending work or publishing a new target.
- Added shared owner/supervisor fixtures cover recovery without redelivery,
  still-pending sweeps, timeout, in-flight-send shutdown, stopped startup and
  stopped renewals, plus bounded panel/row passes and status parking. These Rust
  fixtures remain uncompiled. Production `serve` still passes no self-role
  service, and `CommandRuntime::from_env` still creates none; approved boot
  composition must share one Arc across both paths. Terminal superseded audits
  and their committed-target repair are not covered by processing discovery;
  those and the unknown-work lifecycle remain activation blockers.

Interrupted remote work stays explicitly unresolved; its durable continuation/
reconciliation lifecycle must be wired before activation, not silently cleared
by a timer or member snapshot. Added Rust regressions remain unverified locally
while the mandated bounded Cargo pool is unavailable.
The REST regression
target is `two-bot-discord --test self_roles_rest`; admission coverage is
`two-bot self_role_runtime:: -- --include-ignored --test-threads=1`, opted in by
CI's isolated `self-role-store` service job. Neither needs a real token or guild.

A Common Changelog entry and Conventional Commit feature checkpoints provide
release notes. The release standard owned by
[TOG-10035](/TOG/issues/TOG-10035) is merged, and the flow published
[v0.2.0](https://github.com/TogetherWeOwn/two-bot-next/releases/tag/v0.2.0).
This slice's entry remains under Unreleased; it was not part of that release.
Do not manually bump Cargo versions, publish tags or create a release.
Non-author exact-SHA review and green required CI still gate squash merge.
No production guild, token, staging database or real Discord endpoint was
used to verify this seam.
