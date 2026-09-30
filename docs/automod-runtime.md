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
2. Build `DeliveryKey::from_delivery`. Its SHA-256 fingerprint excludes receipt
   time; creates key on message identity, edits on stable revision/facts. Text
   is not persisted. Acquire `AutomodStore::claim` **before** `inspect` changes
   repeat history. `InFlight` and `Replayed` must not inspect, award, or send
   effects again; use `FunnelDisposition::None` for their cache-only handling.
3. `AutomodRuntime::inspect` returns explicit acceptance, match, fetch, ignore,
   or unavailable. Do not turn unknown/failure into ordinary acceptance. A
   matched create is `CaptureOnly`, even in dry-run; a clean create is `Accept`.
   Updates always use `None`, never another message award.
4. For an enforcing match, resolve authoritative `TargetFacts`, including owner,
   bot identity, bot/staff protection, roles, bot permissions and hierarchy.
   Call `target_gate` **before** counting or deleting. Unavailable facts emit
   nothing and count nothing. Resolved protected matches retain legacy counting
   but have no effects. Dry-run fetches no target and never touches the ledger.
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
   Inspection also sweeps inactive authors. No private ticker is introduced.

The caller must serialize repeat-history observations in gateway order. Do not
hold a synchronous pipeline mutex across an await. Target policy/activation
approval must come from the authoritative shared resolver/configuration, never
from `TWO_AUTOMOD_ENFORCE` itself.

## Persistence and recovery

Apply migrations `0220` and `0221` via the existing cutover migration runner.
The legacy table/column names remain unchanged. Delivery claims are separate
from the legacy processed-message ledger. Claim capabilities fence stale
completions and safe reacquisition; keep them internal and do not log them.

There is deliberately no expiring mutation lease. A crash/timeout after the
mutation fence leaves an in-flight claim requiring recorded reconciliation,
not automatic resend. Durable ledger insertion and Discord execution are not a
single transaction: a crash after counting but before sending is also a
reconciliation case, not proof that Discord acted. Duplicate suppression favours
no repeated sanctions over pretending exactly-once remote execution.

Pre-count resolver failures may release an unmutated claim. An unavailable
inspection must not award XP. No automatic recovery, retention deletion, or
claim reset is included here. Shared integration must make the funnel's own
writes idempotent for crash recovery and mode transitions; the synchronous S3
in-memory pipeline is not a durable production store.

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
- DB assertions cover 20-way claim contention, edit contention, replay,
  stale-token fencing, dry-run refusal, uncertain-mutation retention, three
  processed messages and a final violation count of three.

Commands (existing Rust installation, target directory outside the synced tree):

```sh
cargo test --workspace --all-features --locked
cargo test -p two-bot-core --features db --locked --test automod_store -- --ignored
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo fmt --all -- --check
```

REST/mock-executor call assertions, real shard orchestration, durable production
funnel integration and staging soak remain the follow-up's acceptance gates.
