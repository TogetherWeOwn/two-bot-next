# Automod runtime handoff (TOG-10089)

## Delivery boundary

This slice supplies domain decisions, durable claims/legacy counters, Twilight
translation/enrichment, and a handoff into the existing S3 funnel pipeline. It
**does not enable automod in the shard runner or send Discord mutations**. The
shared REST executor (TOG-10076) and shared runtime integration must land first.
There is no private HTTP client, dispatcher, shard, or timer in this slice.

The matcher is the reviewed implementation merged in PR #14. Behaviour references
are legacy `src/automod/service.ts`, `src/automod/store.ts`, migrations
`0013_automod.sql`/`0014_automod_idempotency.sql`, and `src/discord/client.ts`.
No legacy or production service is changed.

## Contract for the shared async gateway

1. Translate with `two_bot_discord::automod::event_to_automod`. Ignore DMs and
   apply the configured guild/activation fence before fetching. A non-bot create
   without member roles, and every update, requires authoritative enrichment.
   Fetch the exact channel/message and resolve that author's guild roles; pass
   them to `with_fetched_message`. Failed lookups are **not** empty roles/content.
   A create awaiting roles retains its immutable gateway facts in
   `create_pending_roles`, with no inspectable snapshot/key until resolution.
   Enrichment replaces only role IDs, checking the author identity too; content,
   mentions, attachments, message time and revision remain the original CREATE.
   A later REST revision belongs to an UPDATE, not to the original create's
   funnel decision.
   Route a MESSAGE_UPDATE dispatch's raw `d` object through
   `PartialEdit::from_dispatch` + `partial_edit_delivery` **before**
   `twilight_gateway::parse`: minimal edits omit the fields a full Twilight
   `Message` requires and fail decoding upstream of enrichment. The delivery
   carries no snapshot, so `inspect` returns `FetchMessage` for it.
2. Build `DeliveryKey::from_delivery`. Its SHA-256 fingerprint excludes receipt
   time and mutable member roles; creates key on message identity, edits on
   stable message revision/facts. Current roles remain authoritative for fresh
   inspection exemptions and target protection, not retry identity. Text is
   not persisted. Acquire `AutomodStore::claim` **before** `inspect` changes
   repeat history. `InFlight` and `Replayed` must not inspect, award, or send
   effects again; use `FunnelDisposition::None` for their cache-only handling.
   An enforcing released pre-count claim replays as `Preserved(claim, match)`
   instead: after `inspect` returns an **enforce-mode** match, the gateway calls
   `AutomodStore::preserve_match` immediately, before target resolution, so a
   same-revision retry replays the stored IDs/reason code without re-running
   the mutable in-memory repeat tracker that unrelated traffic may have swept.
   **Dry-run skips preservation** (the store rejects it for dry-run claims).
   `Preserved` must not inspect; it still reconciles through target
   resolution, counting and planning with its rotated claim.
3. `AutomodRuntime::inspect` returns explicit acceptance, match, fetch, ignore,
   or unavailable. Do not turn unknown/failure into ordinary acceptance. A
   matched create is `CaptureOnly`, even in dry-run; a clean create is `Accept`.
   Updates always use `None`, never another message award.
4. For an enforcing match, resolve authoritative `TargetFacts`, including owner,
   bot identity, bot/staff protection, roles, bot permissions and hierarchy.
   Call `target_gate` **before** counting or deleting. Unavailable facts emit
   nothing and count nothing. Resolved protected matches retain legacy counting
   but have no effects. For dry-run, use `plan` with `ViolationRecord { count: 0,
   inserted: false }` and no target. Its `DryRun` plan preserves capture-only for
   matches and emits no effects. Handle that funnel disposition once and complete
   the claim with `StoredOutcome { matched: true, deleted: false, outcome:
   CompletionKind::DryRun }`. Do not preserve, count, fetch a target or mark a
   mutation for dry-run.
5. For a resolved enforce match, call `record_violation` with the acquired claim.
   Its insert-first transaction counts a `(guild_id, message_id)` once across
   edit revisions. Pass the returned `ViolationRecord` to `plan`.
   `AlreadyProcessed` has no effects. Otherwise the plan deletes the exact
   message first, then follows the default ladder: delete, warn, 600 s timeout.
   Warn/timeout permission or hierarchy refusal does not undo exact deletion.
6. Commit `mark_mutation_started` before the first Discord mutation. A false or
   failed result means send nothing. Execute only through the shared executor,
   using its moderation no-auto-retry/abort policy and mention suppression.
   Never infer REST success from a plan or turn an uncertain response into a
   success receipt. Follow-up failure cannot release a started claim.
7. Call `Pipeline::handle_with_message_disposition(event, disposition)` once,
   **instead of** also calling `handle`. It preserves existing cache/lifecycle
   handling. Capture-only reaches `FactsSink::record_message` but skips activity,
   XP and milestones. The shared dispatcher must also suppress any downstream
   text automations for rejected creates. No such automation runner is added
   here. Existing `Pipeline::handle` retains S3 behaviour until integration.
8. Complete the claim with an actual typed `StoredOutcome`; `deleted` is a
   confirmed receipt, not an intention. Different enforce/dry-run claims allow
   enforcement after preview: integration must independently keep funnel/facts
   once-per-message when modes change. Do not replay ingestion just because an
   enforcing claim is new.
9. Invoke `expire_repeat_history(now_ms)` from the shared maintenance tick.
   CREATE inspection also sweeps inactive authors on the message clock; an
   idle tick never sweeps ahead of the newest CREATE observation. Updates do
   not advance expiry, replace, insert or prune CREATE history. Stamped
   updates inspect the bounded interval `[edit time - window, edit time]`,
   excluding future rows and their own message ID without discarding them.
   Only CREATEs enter the bounded tracker, which retains the newest CREATE
   observations by timestamp. A REST-fetched future edit cannot remove an
   original CREATE row needed by still-queued older deliveries, even if its
   content changed. Edits evaluate their current content against CREATE
   history; they are not additional messages. Unstamped updates likewise
   evaluate at receipt time without recording or pruning repeat history.
   They still re-inspect all other filters and never award XP. Same-revision
   enforcing matched retries must use the durable preserved decision rather
   than assume mutable repeat history remains unchanged. No private ticker
   is introduced.

The caller must serialize repeat-history observations in gateway order. Do not
hold a synchronous pipeline mutex across an await. Target policy/activation
approval must come from the authoritative shared resolver/configuration, never
from `TWO_AUTOMOD_ENFORCE` itself.

## Persistence and recovery

Apply migrations `0220`–`0223` via the existing cutover migration runner.
The reviewed database-role matrix grants runtime CRUD on the three automod
relations, with no web-reader access, DDL or broad future-table grants. Matrix
coverage includes these migrations, and the scratch role fixture exercises
claim/ledger access as the non-owner runtime and reader/DDL refusal.
The legacy table/column names remain unchanged. Delivery claims are separate
from the legacy processed-message ledger. Claim capabilities fence stale
completions and safe reacquisition; keep them internal and do not log them.
Migration `0222` records the counted phase on the claim atomically with the
ledger commit: a counted claim survives `release_unmutated` as reconciliation
evidence, and a retry replays it (`InFlight`) instead of acquiring a fresh
claim that would plan `AlreadyProcessed` with no effects. Migration `0223`
adds the preserved pre-count decision to the claim (`matched_filter` plus the
four subject IDs, all IDs/reason code only, never message content) with a
`released` handoff flag: `preserve_match` is owner-gated and idempotent, set
once per active unmutated claim; `release_unmutated` marks a decided claim
released instead of deleting it; the same-revision retry rotates the claim
token and replays `Preserved(claim, match)`, while a concurrently owned
unreleased row stays `InFlight` and an unknown stored filter name never
invents a match. Stale tokens cannot start mutations, complete, or count.

There is deliberately no expiring mutation lease. A crash/timeout after the
mutation fence leaves an in-flight claim requiring recorded reconciliation,
not automatic resend. Durable ledger insertion and Discord execution are not a
single transaction: a crash after counting but before sending is also a
reconciliation case, not proof that Discord acted. Duplicate suppression favours
no repeated sanctions over pretending exactly-once remote execution.

Pre-count resolver failures may release an unmutated claim. When the claim
carries a preserved decision, a same-revision retry replays the stored
IDs/reason code instead of re-running the mutable in-memory repeat tracker,
which unrelated traffic may have swept in the meantime. An enforcing matched
claim must not be released before preservation succeeds; a false/error result
is a fail-closed integration error, not permission to re-inspect and award.
The replay must still reconcile through target resolution, counting and planning.
An unavailable inspection must not award XP. No automatic recovery, retention
deletion, or claim reset is included here.
Shared integration must make the funnel's own writes idempotent for crash
recovery and mode transitions; the synchronous S3 in-memory pipeline is not
a durable production store.

## Shared activation orchestrator (TOG-10261)

`two_bot_discord::automod_activation::AutomodActivation` is the one call the
shared async gateway makes per translated delivery, before the funnel:
`process(delivery, at_iso)` returns the `FunnelDisposition` to hand once to
`Pipeline::handle_with_message_disposition` plus a typed outcome. It owns no
client, router, timer or task; every read and mutation goes through the shared
`ActionExecutor`, and the runtime mutex is never held across an await.

1. Outside the fence (`admits`) → `Bypassed`, ordinary funnel, no claim.
2. A delivery without a snapshot (partial edit, role-less create) is enriched
   from `GET /channels/{c}/messages/{m}` and the author's member roles. A failed
   lookup is `Unavailable` (create → `CaptureOnly`, update → `None`), unclaimed.
3. The claim comes before inspection. `InFlight`/`Replayed` → `Duplicate`
   (`None`): nothing inspected, awarded or sent. `Preserved` replays the stored
   decision straight into enforcement.
4. Dry-run settles `DryRun` with no preservation, target fetch, count or call.
5. Enforce preserves the match, resolves target facts (guild owner, Owen,
   configured and dangerous-permission roles, hierarchy, bot permissions; any
   unknown role fails closed), counts once, plans, commits the mutation fence,
   then deletes. A `Rejected` delete settles `SanctionRefused` with no ladder
   step; any other delete failure is retained (`UncertainDelete`). Warn is the
   counted ledger row only (no Discord call). Timeout follows the confirmed
   delete; an uncertain timeout is retained (`UncertainTimeout`).
6. Only real typed receipts complete the claim. Retained claims are never
   retried, leased or resent; they wait for recorded reconciliation.

`RestAutomodFacts` is the production `AutomodFacts`; `AutomodStore` is the
production `AutomodClaimLedger`.

## Gateway loop wiring (TOG-12354)

`crates/bot/src/automod_gateway.rs` plus `gateway.rs` wire the orchestrator
into the one shared async gateway loop. Active only when `TWO_AUTOMOD=1`;
otherwise the loop is unchanged.

- **Order.** In the serial dispatch worker, per dispatch and before the funnel,
  `process` runs once for the translated delivery, bounded by 12 s (a timeout
  keeps a create capture-only and leaves the claim for reconciliation). The
  returned disposition goes to `handle_at_with_message_disposition` exactly
  once; `handle`/`handle_at` is never also called.
- **Partial MESSAGE_UPDATE.** At reception the raw `d` object is decoded with
  `PartialEdit::from_dispatch` + `partial_edit_delivery` BEFORE
  `twilight_gateway::parse`. A parse failure is tolerated only for a dispatch
  that decoded this way; there is then no event for the funnel and nothing to
  award. Other parse failures stay fatal.
- **Production wiring.** `AutomodStore` ledger, `RestAutomodFacts` and the
  command runtime's `ActionExecutor` (a private executor is built only when the
  command runtime is parked). Owen comes from `TWO_OWEN_USER_ID` and protected
  roles from `TWO_MODERATION_PROTECTED_ROLE_IDS`, never from
  `TWO_AUTOMOD_ENFORCE`. The scope is the configured guild with no live
  approval, so only the staging guild is ever inspected. An enabled but invalid
  configuration (including a missing Owen id) fails gateway start rather than
  running unmoderated.
- **Maintenance.** `expire_repeat_history` runs on the existing periodic-job
  supervisor (`automod_expiry`, every 60 s, no I/O); no private timer.
- **Text automations.** The command runtime's message hook is no longer fired at
  reception for creates while automod is active; the worker fires it only for an
  accepted create (`Accept`) or a direct message, which has no disposition. Matched,
  unavailable and timed-out creates are rejected for automations as well as for
  XP/activity.
- **Funnel once-per-message.** The funnel's writes commit with the gateway
  checkpoint, so a dispatch reaching `process` is not yet in the funnel. A
  replay after a crash between the claim write and the checkpoint finds a
  settled claim (`Duplicate(Some(receipt))`) or an unsettled one
  (`Duplicate(None)`) and restores the funnel once from the receipt via
  `Activation::uncommitted_disposition` (matched → capture only, clean → accept,
  unsettled → capture only). No Discord effect is resent. A dry-run to enforce
  change owns a new claim and the same rule applies, so a message is funnelled
  once either way. Covered by `crash_before_checkpoint_restores_the_funnel_once_without_resending`
  and `mode_change_after_crash_funnels_the_message_once`.

Staging soak remains a separate gate; this change supplies no live approval.

## Gates and evidence

- `TWO_AUTOMOD=1` enables inspection; absent `TWO_AUTOMOD_ENFORCE=1` is dry-run.
- Scope is staging `1545644954272137297`; live `326474832151838730` also requires
  explicit externally supplied approval. This code supplies no live approval.
- Domain tests cover pre-delete protection, dry-run, sanction planning, blocked
  attachments, exemptions, fetch requirements, stable retry identity, edits and
  once-per-message effect suppression.
- Twilight tests use in-memory mock messages; the shared-pipeline test observes
  facts, XP, activity and milestone calls. These prove the handoff, **not** live
  delete/warn/timeout execution or staging deployment.
- The ignored DB test uses only `agent_test@agent-testdb:5432/agent_test` (empty
  password), or the explicit GitHub Actions Postgres service container. It has
  no `DATABASE_URL` fallback and creates/drops only its isolated test schema.
- `crates/discord/tests/automod_activation.rs` drives the orchestrator over the
  scripted mock REST double: dry-run sends nothing, protected targets are
  untouched, the delete/warn/timeout ladder, duplicate delivery once with one
  XP award, partial-edit fetch and re-inspection with no award, uncertain
  delete/timeout retention, refused fence, rejected delete and preserved retry.
- DB assertions cover 20-way claim contention, edit contention, replay,
  stale-token fencing, dry-run refusal, uncertain-mutation retention,
  counted-claim reconciliation, preserved pre-count decision replay, five
  processed messages and a final violation count of five.

Commands (existing Rust installation, target directory outside the synced tree):

```sh
cargo test --offline --locked -p two-bot-core -p two-bot-discord --test automod_runtime --test automod_translation --test automod_activation
cargo test --offline --locked -p two-bot-core --features db --test automod_store --test automod_preserved_replay -- --ignored
cargo clippy --offline --locked -p two-bot-core -p two-bot-discord --features two-bot-core/db --lib --test automod_runtime --test automod_translation --test automod_activation --test automod_store --test automod_preserved_replay -- -D warnings
cargo fmt --all -- --check
```

Staging soak remains the separate acceptance gate. The gateway test double
for the loop itself is the unit coverage in `crates/bot/src/automod_gateway_tests.rs`.
