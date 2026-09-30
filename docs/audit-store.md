# Durable operational audit store

[TOG-10344](/TOG/issues/TOG-10344) adds `two_bot_core::audit_store`
behind the existing `db` feature. This is **storage only**: no client, service,
router, worker, Discord request, readiness claim or runtime activation.

## Migration allocation and compatibility

At main `32ef2b62226e9778ec879bc0d019b88bcfe1b4f7`, the checked-in migration
set was `0001`, `0002`, `0150`, `0160`, `0190`, `0330`. The S5
[delivery ledger](/TOG/issues/TOG-9810#document-delivery) reserves jobs
`0300–0309`, probe/scorecard `0310–0319`, sessions `0320–0329`, and settings
`0330–0339`. No audit number was allocated. This slice reserves **0340–0349**
for audit and adds only `crates/cutover/migrations/0340_operational_audit.sql`.
No shipped migration is changed, renumbered, or deleted. The existing embedded
cutover runner discovers it automatically.

The migration reuses `operational_audit_log` and `audit_kill_switch` from frozen
legacy two-bot `b0a26a5e3882dd0784d208079f309893e2ede7e8`, including timestamp,
string-snowflake, destination, nonce, search-boundary, message-ID and mirror-check
columns. New columns are `delivery_generation` and `delivery_accepted_at`.
Migration replay preserves populated legacy rows and the presence-based halt.
Existing delivered rows stay delivered; no historical delivery is requeued.
Legacy `rota_notice` rows are retained but excluded from claims/queue discovery.

## Minimal downstream API

Construct `AuditStore::new(&PgPool)`. Calls return `AuditStoreError`; they do not
catch database errors and infer send permission. The caller must use classified
metadata-only `AuditEvent` values (IDs, counts, flags and bounded classifications),
never raw content, names or reasons. JSON object shape and kind/channel agreement
are validated; JSON text is stored verbatim, preserving insertion order. Event
instants round-trip as UTC ISO strings with millisecond precision, matching legacy
`Date.toISOString`. Snowflake strings are never cast to database integers.

- `record(event, mirror_channel_id) -> bool`: one atomic event + pending-delivery
  insert. `true` means inserted, `false` means replay; **Err means stop before
  any delivery**. First facts, metadata, destination and nonce win forever.
  Unlike legacy moderation enrichment, duplicates never merge metadata.
  Empty/absent mirror means store-only (`none`); a later replay cannot reroute it.
- `get(entry_id) -> Option<StoredAudit>`: durable facts and delivery metadata,
  including attempts, boundary, accepted message ID/time, and completion time.
- `pending_ids() -> Vec<String>`: discovery only, maximum 25; excludes terminal,
  store-only and unsupported kinds. Each candidate still requires a claim.
- `claim(entry_id) -> Option<AuditClaim>`: five-minute lease with opaque random
  owner token and monotonic generation. Only the winning UPDATE returns a claim.
  A claim exposes a read-only row and `intent()`, never public fencing fields.
- `prepare_send(claim, search_before) -> PrepareSend`: `Prepared` is the only
  successful send preparation. It commits the boundary, attempted timestamp and
  incremented attempt count **before** a POST. `Halted` or `LostClaim` means no
  POST. Repeat preparation, expired owners and reconciliation claims are refused.
  `0` is the empty-history cursor, not a missing boundary.
- `note_accepted(claim, message_id) -> bool`: persist the exact ID returned by
  Discord or independently verified through recovery. First acceptance wins;
  repeat/conflicting IDs cannot replace it. Requires an active, prepared fence.
- `complete(claim) -> bool`: only persisted acceptance can become delivered.
  Clears ownership, preserves evidence, sets completion time. Completion is
  terminal: no API reclaims, releases, quarantines or rewrites a delivered row.
- `release_unattempted(claim) -> bool`: releases preflight/held claims only when
  no boundary or acceptance exists. Does not count a send attempt.
- `fail_attempt(claim, DeliveryFailure) -> bool`: authoritative
  `DefinitelyRejected` (including a guaranteed not-sent request) clears the
  sending owner's boundary and permits retry. `UncertainAcceptance` retains it
  and permits **reconciliation only**. A reconciliation worker cannot use a
  read rejection to clear an earlier POST's ambiguity. Accepted IDs cannot be
  cleared by either path. Attempt count was already persisted at preparation;
  release/completion/recovery do not count it again.
- `renew(claim) -> bool`: only a still-active owner can renew. An expired worker
  cannot revive a lease or affect a replacement owner.
- `quarantine(claim, QuarantineReason) -> bool`: terminal hold with bounded
  classification and retained evidence, never resend eligibility. No automatic
  quarantine reset is exported. Reasons: marker missing, permission revoked,
  evidence conflict.
- `delivery_halt() -> Option<DeliveryHalt>`, `engage_halt(actor_id) -> bool`,
  `disengage_halt() -> bool`: persistent presence-based switch; first engagement
  identity/time wins. Every claim and send preparation reads current switch
  state, not a cached process snapshot. Holding does not delete durable rows.

A `false` owner write is a stale/repeated/refused transition, **not** success.
It never authorizes a subsequent send. Returned `StoredAudit` is data, not a
send capability. Opaque fencing prevents cross-generation writes, not misuse of
a deliberately cloned claim by one consumer; downstream must keep one POST
executor per successful preparation.

## Crash and halt protocol for the service slice

1. Route using the existing core guild/source/destination policies; record first.
2. Claim. `Send` means unattempted; `Reconcile` means a boundary or accepted ID
   already exists. A lease expiring never removes that evidence.
3. For `Send`, preflight the private mirror permissions/history and persist
   `prepare_send` before requesting Discord. Check the persistent halt again
   **immediately before each POST**. If halted before preparation, safely release
   unattempted ownership. If it becomes halted after preparation and the caller
   guarantees no POST began, record definite non-acceptance; the row is retained.
4. Use the stored deterministic nonce with enforcement and disabled mentions.
   On acceptance persist `note_accepted`, then `complete`. If either DB write
   fails, do not send again: the durable boundary survives for recovery.
5. A new owner with `Reconcile` must never POST, even outside Discord's nonce
   deduplication window. Persist an independently verified identity match or
   finish the already-recorded accepted ID; otherwise quarantine. A timeout is
   not evidence of rejection, nor is a missing/edited marker evidence of no POST.
6. Halt blocks new claims/preparation. An existing owner may still persist
   acceptance/completion or quarantine; these writes are not Discord sends.
   Halt reads and updates are fresh each call. No in-memory halt cache exists.

The core's explicit legacy **switch-read failure is fail-open** decision remains
unchanged. The store returns the read error so the service can classify/log it
using that core model. This is separate from failed durable recording,
preparation or fencing: those always prevent a POST. Database and Discord cannot
share a transaction; storage does not promise a halt will revoke a POST already
in flight. The final pre-POST switch check is still downstream acceptance.

## Verification and authoritative sources

Explicit DB tests select only `agent-testdb:5432`, user `agent_test`, empty
password, or the disposable CI Postgres service. They never read application
`DATABASE_URL` or substitute credentials. Each test owns an isolated schema and
uses separate pools for worker races/restart. Connection/setup failures fail tests.
CI explicitly executes the ignored DB lane; ignored tests alone are not evidence.

```sh
cargo test -p two-bot-core --features db --lib audit_store --locked -j 1
cargo test -p two-bot-core --features db --test audit_store --locked -j 1 -- --ignored
cargo clippy -p two-bot-core --features db --all-targets --locked -j 1 -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Tests cover clean embedded-runner migrations/replay, populated legacy upgrades,
insert replay and races, concurrent claims, stale/repeated writes, accepted-ID
restart, expired/ambiguous claims, definite rejection, quarantine, bounded queues,
persisted halt and a failed durable write with a fake (zero) POST count. No real
Discord request or production/staging database is used. The full suite is CI-owned.

Sources for the SQL/driver patterns:

- [sqlx 0.9 transactions](https://docs.rs/sqlx/0.9.0/sqlx/struct.Transaction.html):
  bound queries use the transaction connection, explicit commit, rollback on drop.
- [Postgres UPDATE RETURNING](https://www.postgresql.org/docs/current/sql-update.html):
  returns only rows actually updated; claims never reread another owner's token.
- [Read Committed UPDATE](https://www.postgresql.org/docs/current/transaction-iso.html#XACT-READ-COMMITTED):
  a waiting updater re-evaluates its predicate against the committed row version.
- [Postgres locking](https://www.postgresql.org/docs/current/explicit-locking.html):
  transaction-scoped advisory locks serialize switch changes even when its row
  is absent; owner writes use conditional updates on token and generation.
- [Frozen legacy store](https://github.com/TogetherWeOwn/two-bot/blob/b0a26a5e3882dd0784d208079f309893e2ede7e8/src/audit/store.ts):
  table meanings, deterministic nonce, durable recovery boundary and halt presence.

The mirror/reconciliation service ([TOG-10345](/TOG/issues/TOG-10345)) and canonical
runtime wiring ([TOG-10346](/TOG/issues/TOG-10346)) remain unimplemented by this PR.
