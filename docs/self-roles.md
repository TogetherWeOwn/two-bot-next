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
  accepted for a role mutation; other 2xx/3xx are uncertain. Singular mutation
  status is captured from received headers on the same transport without reading
  the unused provider body: a stalled/truncated error body cannot invent a lost
  response. Member/policy reads still require complete bodies; no status retry is
  added. Received 5xx remains ambiguous as to effect, not an unknown in-flight send.

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
  Definite new attempts resolve only their own role/direction evidence, preserving
  older same-direction uncertainty and unrelated/opposite-direction evidence.
  Both processing and terminal steps apply this rule before observation; an older
  pending send must not cause a fresh acknowledged or rejected attempt on another
  role/direction to be mislabeled as unresolved.
  Recovery retains pending state, switches to rollback, and preserves unresolved
  evidence even when a read looks restored. Pending audits cannot settle or
  publish success. Generation transfer refuses old workers' flag/effect writes.
- Migration `0205_self_role_exchange_receipts.sql` adds a distinct send identity
  and redacted receipt capability per role/direction attempt, including repeated
  attempts on the same role. Processing and terminal journal APIs commit the
  ticket and unioned attempted/unresolved role evidence together, require
  initialized intent and all supplied live fences after panel -> event waits,
  and do not authorize a send without the executor's next ownership check.
  A surviving original sender can persist a received final HTTP status or definite
  no-send after generation transfer, but only to its own ticket. Exact replay is
  idempotent; contradictory receipts are refused. That API cannot write audit
  effects/outcome/claims or the panel target. A received 5xx is response provenance,
  not a no-effect verdict; cancellation/timeouts leave the ticket pending.
  Pending tickets also gate aggregate checkpoint clearing and both settlement/
  terminal-completion paths. Aggregate writes acquire the audit lock before the
  statement that reads pending tickets, so a pre-wait snapshot cannot erase a
  concurrently committed journal. They union unresolved role/direction IDs from
  pending tickets rather than trusting a replacement effect snapshot. Lock-wait
  source fixtures cover processing/terminal checkpoints and post-wait journal
  expiry. Legacy pending work is never assigned synthetic
  provenance. Normal processing and typed terminal runtime steps now create
  tickets in the shared paced journal callback and persist definitive receipts
  before any aggregate write can fail after generation transfer. The shared
  executor carries raw final status independently of result mapping/ownership;
  outer pre-send errors become no-send receipts only when a ticket was committed.
  Inner timeout/transport ambiguity leaves the ticket pending. Receipt-write
  failure stops the step without a retry or invented completion. Stale-maintenance
  handoff now enters that same typed terminal path with freshly claimed evidence
  ownership, not a fabricated live processing event or lane-only send authority.
  Receipt completion alone does not clear aggregate uncertainty or establish
  convergence. Live-fenced current-owner incorporation now merges cumulative
  attempts and only acknowledged 204 compensation receipts, preserving observed
  snapshot fields, every inherited unresolved ID and the persisted pending flag.
  Pending tickets add their direction evidence and force pending; received 5xx
  and other ambiguous statuses add effect uncertainty without fabricating an
  unknown in-flight send or acknowledged compensation. The processing path checks
  its live event and optional normal lane; terminal incorporation requires a fresh
  typed evidence owner and the live committed maintenance lane. Both sample time
  after panel -> audit waits and read receipts in a new post-lock statement.
  Neither path changes outcome, immutable intent, ownership, expiry or target.
  Recovery synchronizes effects/pending before planning or observation and rejects
  catalogue drift in incorporated evidence. Replays retain cumulative facts.
  The incorporation APIs remain conservative; they do not retire uncertainty.
  Migration 0206 below supplies separate attribution and retirement. The stale
  handoff uses a fresh typed owner; no timer or optimistic snapshot substitutes
  for sender provenance.
  Boot stays disabled. Isolated source fixtures cover
  processing/terminal generation transfer, same-direction ticket separation,
  no-send and 204/403/429/500 receipts, exact/contradictory replay, invalid status,
  uninitialized/stale journaling, snapshot-free owner-state preservation and
  pending settlement refusal. The explicit role matrix includes the new relation;
  its source fixture exercises runtime CRUD and web-reader denial. Runtime source
  regressions assert late 204/403/429/500 receipt persistence after processing or
  terminal generation transfer without replacement audit/target writes, raw status
  retention despite truncated/stalled bodies, definitive post-journal no-send,
  no ticket before journaling, and pending receipts across timeout/cancellation.
  Additional incorporation source fixtures cover acknowledged compensation versus
  refusal/ambiguity, idempotent replay, stale event/lane refusal, terminal post-lock
  expiry, unchanged authority metadata and retained pending settlement gates.
  Processing runtime recovery synchronizes the late evidence without new sends.
  These Rust fixtures remain uncompiled while the required bounded pool is absent.
- Migration `0206_self_role_exchange_baselines.sql` separates legacy uncertainty
  from ticket contributions, preserving the existing JSON-array-as-TEXT format.
  Upgrade backfills **all** pre-existing pending/unresolved work conservatively;
  matching role IDs in 0205 tickets do not prove exclusive attribution. New
  journals capture the pre-ticket aggregate under the audit lock exactly once.
  Replay does not absorb subsequent ticket uncertainty into that legacy floor.
  The floor cannot be retired by a ticket receipt, time, or a member snapshot.
- `retire_role_receipts` and `retire_terminal_receipts` require the current live
  processing/normal-lane or typed-terminal/committed-maintenance fences. They lock
  panel -> audit -> receipt rows and sample time after those waits, including a
  wait on sender completion. Only completed response/no-send rows receive an
  immutable retirement time/generation; pending, cancelled and transport-lost
  tickets remain unretired. Receipt completion alone cannot retire anything.
  Current-owner retirement preserves observed effects, intent, outcome, leases,
  chronology and target. Historical attempts and only 204 compensation remain
  cumulative. A same-role pending ticket and legacy floor survive another
  completed ticket; unrelated unattributed directions fail closed.
- Received 5xx/other ambiguous responses retire unknown-send provenance, **not**
  effect uncertainty or evidence of success. Their unresolved effects remain
  until subsequent authoritative observation with no unknown send pending.
  Replaying retirement neither clears already-retired ambiguous effects nor
  reopens them after observation. A later same-role no-send receipt cannot
  erase earlier ambiguous effects. Aggregate checkpoints union legacy and
  unretired-ticket directions; settlement/completion refuse unretired completed
  tickets too, preventing an aggregate writer from bypassing the live retire fence.
- Processing recovery and normal checkpoints synchronize retired evidence into
  runtime state. Typed terminal recovery/steps also refresh the opaque claim's
  acquisition snapshot after commit, preventing stale preservation from reviving
  its retired uncertainty. The former lane-only stale path now hands immutable
  discovery metadata to `recover_terminal`, which reloads evidence under a fresh
  typed claim before acquiring a maintenance lane and performing any repair read
  or send. It never copies the obsolete prepared intent/effects into that owner,
  and does not update the obsolete prepared snapshot to impersonate repair output.
  Normal `execute_step` no longer accepts a maintenance-lane alternative. Every
  new repair send uses the shared terminal journal/receipt/retirement path. The
  compatibility journal remains a storage seam for untracked legacy evidence and
  still pins a conservative floor; existing floors are not retroactively assigned
  tickets or retired by the handoff and may remain permanently uncertain.
- Added uncompiled Rust classifier/store/runtime fixtures cover completed versus
  same-role pending tickets, legacy overlap, no-send/204/403/429/500/2xx/3xx facts,
  replay/contradiction, former-owner/lane refusal, post-receipt-lock expiry,
  unchanged authority and typed claim refresh. The explicit role inventory and
  runtime CRUD/web-reader-denial source fixtures include the baseline relation.
  Extracted SQL-only fixtures exercise the migration and persisted unions in an
  owned `agent-testdb` schema, with expected classifier arrays supplied explicitly;
  they do not execute the Rust classifier, API transactions, runtime or grants.
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
  composition must share one Arc across both paths. Processing discovery still
  refuses terminal rows; the separate terminal consumer below repairs only to
  committed targets. The unknown-work continuation contract below remains subject
  to compiled acceptance before activation.

### Unknown-work continuation contract (not activation evidence)

The existing service/supervisor keeps unknown work durably discoverable, not
successful or discarded. A bounded sweep can repair drift to the committed target
while retaining the original pending ticket; a later sweep with that target already
restored performs fresh reads but does not resend the acknowledged repair. Each
attempt drops its renewal owners and releases only its own lane. Persisted expiry
provides backoff; discovery rotation/interleaving lets other processing and terminal
work advance. Restart loses only the in-memory cursor, not intent or uncertainty.
A completed sweep is job progress, never a successful role-change result.

Only the original sender's genuine final response or definite no-send receipt can
complete its ticket. Completion does not itself change aggregate evidence or grant
settlement authority: a subsequent fresh live owner retires that receipt and
verifies convergence. A conservative legacy floor has no invented sender identity
and survives all of those operations. Such work remains unresolved indefinitely;
there is no retry-count, elapsed-time, restart, snapshot or acknowledged-repair
shortcut to successful settlement. Disabling the catalogue parks recovery without
deleting its evidence; dry-run never acquires terminal repair ownership.

The uncompiled `service_continues_unknown_work_without_redelivery` fixture exercises
three service sweeps with a same-direction pending ticket, acknowledged repair and
optional overlapping legacy floor. It asserts lease backoff, generation rotation,
progress for fresh processing rows, unchanged winner chronology, one total repair
send, genuine sender completion without aggregate authority, and continuation after
service restart. The ticket-only case completes a terminal convergence receipt;
the legacy case remains pending and discoverable. Neither case reports success for
the superseded event or republishes the target. Rust acceptance and approved shared
boot composition still gate activation; source fixtures are not passed tests.

### Terminal repair storage checkpoint (not runtime activation)

- Migration `0204_self_role_terminal_repair.sql` adds `repair_expires_at` and
  `repair_complete` (32 audit columns after upgrade), plus a configured-source
  partial due index. Processing admission deliberately still refuses terminal
  rows. `superseded_audits` discovers only exact supersession rejections with
  pending/effect evidence and no completed receipt, scoped to guild/panel/message/
  surface, with null/due attempt expiry, stable C-collated ordering and a 32-row
  cap. Discovery is a hint, not authority; claimed expiry yields to other rows.
- `claim_superseded_audit` locks/rechecks the exact metadata, reloads immutable
  initialized intent and all eight effect arrays, and grants a **new** secret
  token/generation in a distinct `SupersededClaim`. It never reads/borrows the
  stored old token or exposes a processing claim. Rejection, original snapshots,
  chronology and exchange uncertainty remain unchanged. Malformed snapshots fail
  closed, including when an initialized empty target would otherwise be valid.
- Explicit terminal ownership/renewal use strict expiry and post-lock database
  time. Journaling additionally requires a live, same-scope **maintenance** lane
  with the unchanged committed target, including committed null/empty. Normal
  event lanes and unknown targets are refused. Paired journal/completion lock
  panel then audit and sample time only after both waits. These store fences do
  not replace executor checks after pacing/journaling or around each REST call.
- Terminal late evidence remains token/generation-fenced, not send authority.
  Attempted/compensated history is cumulative; a late accepted write invalidates
  any prior repair receipt. Inherited unknown exchanges remain pending and their
  unresolved IDs survive repair journaling/response evidence. Acknowledged repair,
  a timer, or an authoritative snapshot cannot prove the original request stopped.
- `finish_superseded_repair` is only a receipt for caller-verified convergence:
  it requires both live fences, initialized intent, and no pending/unresolved
  work. It never makes the superseded event successful or publishes a panel
  target. Completion removes the row from discovery until new accepted evidence.
- Rotation refuses the former worker's later aggregate response writes. Sender
  ticket completion remains separate: 0206 current-owner retirement can resolve
  completed ticket provenance without restoring former-worker authority. Legacy
  or genuinely pending sends can still remain permanently uncertain; there is no
  automatic timer/snapshot retirement. Their continuation remains an activation
  blocker, not a reason to clear the flag; typed stale handoff preserves that floor.
- Added isolated store source fixtures cover scope/due/limit/race discovery,
  fresh secret generation and former-worker refusal, initialized empty targets,
  malformed intent, terminal renewal/expiry, panel-to-audit lock waits, selected/
  empty committed targets, unknown/nonmaintenance lane refusal, inherited pending
  preservation, completion and late-evidence invalidation. The fixtures are
  **uncompiled** while the mandated Cargo pool is unavailable. SQL-only smoke
  evidence does not substitute for Rust acceptance.

### Terminal repair runtime checkpoint (not boot activation)

- `recover_terminal` consumes a freshly claimed terminal hint without fabricating
  a normal `PreparedSelfRole` or reopening processing admission. It refuses
  nonexclusive/wrong-scope input, uninitialized intent and catalogue drift in
  snapshots or any effect array before REST. The dedicated terminal evidence
  owner renews while bounded-waiting for a new maintenance lane; both renewal
  tasks belong to the awaited repair and abort when it is dropped.
- `reconcile_stale` stops the obsolete keepers and releases only their old lane
  fence, then uses `recover_terminal` rather than lane-only aggregate journaling.
  Processing rows, already-claimed evidence, completed repair rows and mismatched
  immutable metadata grant no handoff authority and cause no repair REST work.
  Typed acquisition rotates away former aggregate-write authority; sender receipt
  completion remains independent. Repairs observe fresh policy/member state and
  reconcile only the new lane's committed selected/empty target. A completed
  repair receipt is not success of the obsolete event and publishes no target.
  Added uncompiled source regressions cover late processing 204 handoff to selected
  and empty targets, distinct repair tickets without new legacy floors, active-owner
  and wrong-metadata refusal, misleading obsolete caches, same-direction pending
  overlap and legacy floors across genuine sender no-send completion. Existing
  shared terminal source fixtures retain cancellation, timeout, partial/refused/
  ambiguous response and pre/post-send fence coverage; no Rust execution is claimed.
- A processing audit inserted after a newer lane's bulk supersession can discover
  that it is obsolete before constructing `PreparedSelfRole`. Admission now uses
  `supersede_processing_audit`, not generic refusal/settlement: it locks the panel
  then audit, rechecks stored scope and strictly later distinct-event chronology
  (including legacy snowflake fallback), and checks the live event token/generation
  against a clock sampled after both waits. Only rejection/code/reason/processing
  expiry change; all snapshots, effects, compensation and pending state survive.
  No REST, success reply or target publication occurs in this transition; dirty
  admission still returns an unresolved/pending result, not a no-effect refusal.
  Terminal rows with evidence become eligible for the separate repair queue;
  clean rejections need no repair. Dry-run can record this rejection but never
  claims terminal work.
  Unknown sends remain unknown after the transition and cannot complete a receipt.
- The only repair target is the maintenance lane's **committed** option or
  committed null/empty selection. Unknown targets and removed options fail
  before REST; no target is seeded from the old before/desired intent. Original
  before supplies only the net-evidence baseline. Authoritative shared REST
  snapshots revalidate bot/hierarchy/permissions each step; unrelated roles are
  preserved and singular removals precede additions.
- `owns_superseded_repair` checks both live fences and the unchanged committed
  maintenance target using one clock sampled after panel-to-audit lock waits.
  The shared journaled executor owns pacing and checks the pair after pacing,
  after journaling and after the exchange. Runtime checks bracket snapshot reads
  and follow response-evidence waits. The store's completion receipt repeats
  both fences after its own waits, following fresh convergence verification.
- Send intent is durable before REST. Attempt/compensation history is cumulative;
  received/no-send/unknown outcomes retain truthful evidence. A new unknown send
  is monotonic for this owner, and inherited unknown sends and unresolved IDs
  survive later repair responses and observations. Pending work cannot complete
  a repair receipt. No terminal repair sends a successful old-event reply, changes
  the rejection/immutable snapshots or publishes a target.
- The existing supervised pass now interleaves processing and terminal queues:
  at most **eight panels, sixteen discovery queries, sixty-four fetched hints,
  four considered audits per panel and thirty-two considered audits** per tick.
  Unused slots can serve either queue. Panel start advances before awaiting;
  queue priority flips each full start-panel rotation, avoiding parity pinning
  with even panel counts and cancellation before the second slot. Claimed expiry
  yields unresolved terminal attempts to other work. Dry-run skips terminal
  discovery/acquisition entirely and does not retire pending work as simulation.
- Added isolated source fixtures cover restarted selected/missing/empty target
  repair, remove-before-add/unrelated-role preservation, unknown/uninitialized
  refusal, inherited pending, cancellation-owned renewals and mixed/dry-run
  sweeps. Terminal fault fixtures cover acknowledged removal followed by
  403/429/received-500/timeout addition, later authoritative convergence without
  inventing acknowledged compensation, independent evidence-token/lane transfer
  during a delayed response, and expiry while shared pacing is occupied. A
  generated-schema-only trigger delays the journal UPDATE after its initial
  ownership checks; post-journal expiry must prevent REST, retain attempted
  history, resolve only the new definite no-send and preserve inherited unknown
  evidence. Paired store fixtures cover normal/unknown lane refusal and expiry
  after both panel and audit lock waits. Early-supersession fixtures cover ten
  selected/empty runtime cases (clean/effects/compensation/pending/dry-pending),
  twelve store scope/chronology/generation/preservation cases and three
  post-lock expiry/chronology cases. These are **uncompiled source coverage**,
  not passed Rust acceptance or independent review.

Production dispatch/boot remains disabled. The conservative unknown-work
lifecycle, approved shared boot composition and compiled exact-head acceptance
still gate activation, review and merge.

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
