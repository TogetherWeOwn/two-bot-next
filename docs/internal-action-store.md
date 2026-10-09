# Durable internal-action store

The `db`-feature adapter is a persistence boundary, not an HTTP receiver or a
Discord executor. Its deployment does not release the HMAC custody or staging
readiness HOLD.

## Receiver/executor sequence

1. Authenticate the exact bytes and enforce timestamp skew using the domain
   layer. Never log the body or headers.
2. Burn the authenticated nonce in the durable store **before** JSON parsing,
   allowlist checks or rate buckets, passing the exact authenticated timestamp
   header through to the burn. A live duplicate, a stale attempt or any storage
   error refuses the request. The nonce digest is globally scoped, not scoped
   to the caller. Its exclusive expiry covers the complete inclusive
   timestamp-skew window, `(2 * SKEW_SECONDS + 1)` seconds (241 seconds with
   the current core constants). The burn re-checks freshness against database
   time after any pool/row-lock wait and rolls back when the wait crossed out
   of the skew window — a replay that lapsed mid-wait can never win a second
   burn, so the receiver must not continue on any burn error.
3. Apply key/action buckets, parse and validate caller/action/payload/permissions.
   Derive the request identity from the validated caller, idempotency key, action
   and **exact authenticated payload bytes**. The unique slot is `(caller digest,
   key digest)`; the action and payload digest must match on every retry.
4. Atomically claim the slot and persist a scalar intent audit. **Only a newly
   returned execution claim permits side effects.** Commit failures, ambiguous
   commit outcomes, mismatches, in-flight duplicates and uncertain outcomes never
   permit execution. Replays use the persisted terminal response.
5. Perform the side effect once, then atomically persist a secret-safe typed
   terminal response/status and terminal audit. Return the response only after
   the transaction commits. A storage failure after the Discord call leaves an
   uncertain outcome, not permission to execute again.

Nonce burning and idempotency claiming are separate because a retry uses a fresh
signed nonce but the same idempotency key and payload. Burning before claiming
also consumes mismatched attempts; do not undo a burn on later validation/error.

## Callable API (`two_bot_core::internal_action_store`, feature `db`)

- `InternalActionStore::new(PgPool)` reuses the application's pool; it does not
  connect, migrate or contact Discord.
- `burn_nonce(nonce, timestamp) -> Result<bool, InternalStoreError>` validates
  the same 32-hex-character format as the core and stores only a globally
  unique digest. `timestamp` is the exact authenticated timestamp header: the
  burn rolls back with `InvalidInput` when the attempt is no longer within
  skew at commit time (database clock, whole-second `within_skew` semantics).
  Only `Ok(true)` with a fresh timestamp authorizes the receiver to continue;
  every error is a refusal.
- `RequestIdentity::new(caller, key, action, authenticated_payload)` validates the
  caller/key bounds and core action allowlist, and hashes the exact signed bytes.
  Caller must be an authenticated **stable logical principal**; a rotating HMAC
  key ID should map to that principal rather than create a fresh execution scope.
- `claim(&RequestIdentity, &AuditSubject) -> Result<InternalClaim, ...>` returns
  `Claimed(ExecutionClaim)`, `InFlight`, `NeedsReconciliation`, `Replay(response)`
  or `Mismatch`. All except `Claimed` prohibit execution. An `ExecutionClaim` has
  private fields and is only produced after intent and audit commit.
- `release_proven_not_sent(ExecutionClaim)` requires owning-executor proof that
  the mutation was never dispatched. It consumes the opaque, non-Clone claim on
  every return path, atomically records a `released`/`proven_not_sent` audit and
  transitions a fresh `in_flight` intent to `not_sent` (migration `0352`). Only
  that explicit state permits same-key reclaim, under a row lock with exact
  action/payload **and all original subject scalars** unchanged. Claim refreshes
  its diagnostic timestamps before commit. The original intent/audits and nonce
  burn remain; old ownership cannot be used after consuming release. Audit rows
  are per intent/phase, not per execution attempt: `released` proves at least one
  no-dispatch release, not an attempt count. Reconciliation refuses `not_sent`
  even when aged. Unknown, completed or stale ordinary claims cannot be released.
- `finish(&ExecutionClaim, &TerminalResponse)` commits completion and its audit.
- `mark_unknown(&ExecutionClaim)` preserves the slot and deduplicates the unknown
  audit. It never releases the claim.
- `reconcile(&RequestIdentity, &TerminalResponse, ReconciliationEvidence)` accepts
  unknown/stale records only. It commits a proven terminal outcome, not a lease.
- `claim_discord_event(stable_event_id) -> Result<bool, ...>` atomically burns the
  global event digest. Use a namespaced identity stable across redelivery; do not
  generate an ID per delivery. This is a dedup guard, not a retryable event queue.

`AuditSubject` accepts optional `DiscordId` values for guild, actor, target and
`resolved_role_id`. Role executors pin the allowlist-resolved role in the same
committed intent/audit transaction before REST. Every later audit copies that
original scalar, not a newly evaluated role map. Migration `0351` adds nullable
columns; older intents remain NULL and must not be guessed from current config.
`TerminalResponse` is `Success { resource_id: Option<DiscordId>, affected: u32,
outcome: Option<EventOutcome> }` (HTTP 200) or `Failure(TerminalFailure)` with
fixed codes/statuses: `Malformed`/400, `ActionNotAllowed`/403,
`DiscordRejected`/422, `NoEffect`/502. `EventOutcome` is the closed legacy
result word (`created`/`updated`/`cancelled`, migration `0423`): event intents
always record one so replay returns the first result byte-identically, while
announcement receipts stay `None` and render `message_id`. Failure is
definitive; timeout/transport uncertainty must use `mark_unknown`.
The adapter persists the whole typed response. It intentionally accepts neither
`serde_json::Value` nor `ActionError` (which contains free-text log details).
Receiver/executor follow-ups must map these typed scalars to their wire envelopes
and extend typed response fields if needed, not cache arbitrary provider JSON.

`ReconciliationEvidence` is `DiscordConfirmedEffect` (success only),
`DiscordConfirmedNoEffect` or `ProvenNotSent` (failure only). Evidence must be
independently established; the enum is not proof or an access grant. No free-text
evidence, tokens or provider error details are stored.

The existing pure `authorize` helper still burns an in-memory nonce synchronously.
The receiver slice must preserve its ordering while introducing the async durable
burn between signature/freshness and parsing/buckets; merely calling this store
only after the entire helper is not the final receiver contract. Neither HTTP
route nor this async seam is wired by this PR.

## Crash and reconciliation semantics

A crash after intent commit is conservatively ambiguous, including a crash before
sending anything. The store cannot prove whether Discord applied an effect. The
core stale threshold is a diagnostic cutoff: an old intent requires
reconciliation. It is **never** a lease expiry authorizing a second execution.
An explicitly marked unknown outcome has the same no-reexecution semantics.

Independent reconciliation must prove a terminal outcome using authoritative
Discord evidence (or prove no request could have been sent). It can persist a
terminal result plus a closed evidence code, but cannot reset a slot or issue an
execution claim. A reconciled failure stays a cached failure; a new user intent
needs a new key. Fresh running intents cannot be reconciled. Late completion and
reconciliation serialize under the row lock; one wins, and a subsequent writer
must not overwrite the terminal response.

There is no automatic pruning/reclamation of idempotency slots or Discord event
dedup records. A later retention policy must cover every retry/DLQ/reconciliation
path before deletion can be introduced. Expired nonce digests are replaced only
through the atomic burn operation.

## Data minimization and failures

The schema contains digests, fixed caller/action/outcome/evidence codes, numeric
intent IDs, optional validated Discord snowflakes, timestamps and typed terminal
response fields. No raw request, OAuth token, nonce, key, HMAC header, free-text
reason, provider response/error or exception source is accepted for audit.
Database errors are mapped to fixed public errors without SQLx source text;
callers must not enrich those errors with raw input or credentials.

DB writes fail closed. A lost/ambiguous commit acknowledgement may have persisted
an intent without returning a claim. A retry observes that intent and refuses
execution until reconciliation. This sacrifices availability rather than risk a
duplicate Discord effect.

## Verification boundary

Migrations and integration tests run only in an isolated schema on `agent-testdb`
(user `agent_test`, empty password) or the CI PostgreSQL service container. No
staging/production migration apply, Discord action or deployment is part of this
slice. Unit tests require no database credentials.
