# Operational audit mirror service

[TOG-10345](/TOG/issues/TOG-10345) adds `two_bot_core::audit_service`
(`db` feature) and `two_bot_discord::audit_mirror`. Together they are the
delivery engine for [TOG-9810](/TOG/issues/TOG-9810): events are recorded
before anything is sent, every send is fenced by the persistent kill switch,
and an ambiguous post is reconciled against mirror history instead of
resent. [TOG-12240](/TOG/issues/TOG-12240) runs it in the bot (see
[Runtime](#runtime)); gateway classification and moderation recording are
[TOG-10346](/TOG/issues/TOG-10346). Nothing here touches a production or
staging guild, token or database.

## Pieces

- `two_bot_core::audit_mirror` (no `db` needed): the `AuditMirror` contract
  plus pure helpers — privacy/guild policy, author + exact identity-marker
  matching, snowflake arithmetic and bounded dedup scan limits.
- `two_bot_core::audit_service`: `AuditMirrorService<M: AuditMirror>` with
  `record`, `deliver_entry`, `deliver`, `drain_pending` (≤25 rows),
  `transitions`, and the `with_pre_send_gate` fault-injection seam.
- `two_bot_discord::audit_mirror`: `impl AuditMirror for ActionExecutor`.
  The executor owns pacing, the nonce/`allowed_mentions` wire shape and the
  REST surface; the adapter maps `DiscordError` to `MirrorError` and parses
  channel documents/history strictly. There is no second HTTP client.

## Mirror classification

`MirrorError::Rejected` means provably unsent or refused (4xx or local request
validation): a post retry is permitted under the store's bounded policy; a
refused preflight read quarantines. `RateLimited` (429) defers; `Uncertain`
(timeout, 5xx, connection failure, malformed successful response) keeps
recovery evidence. An unreadable HTTP 200 does not prove permission loss.

`channel_document` reduces `GET /channels/{c}` to `guild_id` plus the
`@everyone` overwrite (type 0 whose id equals the guild id). A missing
`guild_id` yields `""` and is fenced `WrongGuild`; a missing overwrite
array yields `everyone: None` and is fenced `PublicChannel`. Malformed
rows or non-object bodies are uncertain, not permission revocation.
`channel_history` requires every entry to contain a string message ID,
author ID and content (empty content is valid). Both identities must be
canonical nonzero decimal u64 values: no signs, whitespace, leading zeros or
overflow. Message IDs must be unique and strictly descending **numerically**;
the page may contain at most the requested limit (clamped to 1–100), and every
message must be strictly below a supplied `before` cursor. The cursor itself
must be a canonical nonzero u64 identity.

The adapter and the service both apply the shared `validate_history_page`
contract to the **complete** page before using any row. Any malformed row,
oversized page, disorder, duplicate or cursor-equal/newer evidence makes the
entire page uncertain, even if a valid-looking marker appears first. Never
shorten a page by filtering or truncating: its original size, newest ID and
oldest ID establish exhaustion, the recovery boundary and the next cursor.
Diagnostics use fixed metadata-only reasons, not remote identities or content.

## Send protocol

`deliver` dispatches on `claim.intent()`. The send path, in order:

1. Missing destination → `Unclaimed` (store-only rows never deliver).
2. Read the durable halt fresh: halted → `release_unattempted` → `Held`.
   The row stays pending with zero attempts and cannot be reclaimed while halted.
3. Preflight the channel and privacy/guild policy. A refused read or public
   channel quarantines `PermissionRevoked`; a foreign guild quarantines
   `EvidenceConflict`; transient/unreadable evidence defers without a POST.
4. Bounded dedup scan (≤5 pages): page 0's newest ID + 1 seeds the durable
   `search_before` boundary; an empty channel seeds `"0"`. If a bot-authored
   exact marker is found, use **that matching ID** as the boundary instead.
   This admits the known mirror even if adoption's accepted-ID write fails.
   Validate each complete page before choosing that boundary or adopting a
   marker. Invalid evidence defers without preparing an attempt or POSTing;
   a later healthy preflight can retry with the stored deterministic nonce.
5. `prepare_send` persists the boundary and attempt before any POST.
   A halt releases the unattempted row; a lost fence stops the sender.
   Dedup adoption writes `note_accepted` + `complete`, without posting.
6. Run the fault-injection seam, then `post_mirror_checked`. The adapter
   reserves the executor's shared 110 ms pacing lane **before** invoking
   the service's final authorization callback. The reservation is held
   through authorization and the bounded send, preventing another audit
   sender from overtaking a slow DB fence and bursting together with it.
7. The callback reads the halt and calls `check_prepared_send`, which
   revalidates the prepared sender's token, generation, unexpired lease,
   pending evidence and persistent halt under the store's locks. A lost
   fence makes zero POSTs. A halt records definite non-acceptance with
   owner fencing; no pending row is dropped. A failed DB authorization
   returns an error and preserves the prepared recovery evidence.
8. With no further pacing/queue wait, POST the stored nonce and formatted
   metadata-only content. `Ok(id)` → `note_accepted` + `complete` → `Delivered`.
   An empty ID or uncertain error retains the boundary for reconciliation;
   definite rejection or 429 permits the store's bounded retry policy.

The store and Discord do not share an atomic transaction. A process can
still stop at the DB/network boundary; accepted posts therefore always
need durable marker-based recovery. The final fence prevents a worker
already expired/replaced during preflight or pacing from sending.

## Reconcile protocol

A `Reconcile` intent means acceptance is uncertain. A recorded
`mirror_message_id` completes directly with zero Discord reads. Otherwise
scan newest-first until a bot-authored exact marker at/above the boundary
is found. Ambiguous reads preserve the boundary for a later healthy read;
HTTP refusals quarantine `PermissionRevoked`; proven exhaustion or passing
below the boundary quarantines `MarkerMissing`. Reconciliation never POSTs.

Audit recovery scans at most 16 pages or 30 seconds per claim. The nullable
`delivery_reconcile_before` field stores only fully validated, marker-checked
page floors, and a new owner resumes below the saved floor, even after a
restart or new channel traffic. An in-flight read is cancelled at the deadline;
its unexamined page cannot establish a checkpoint. Strict numeric progress
returns `ScanBoundReached`, atomically saves the floor and releases ownership,
and resets the consecutive no-progress budget. The original pre-POST boundary,
nonce, attempt count and acceptance evidence never change. Scan comparisons
and read cancellation use the same Tokio monotonic clock, including the
explicit deadline test seam.

A bound without new progress preserves the cursor, yields and records
`discord_reconcile_scan_bound`. A second consecutive no-progress bound
quarantines `ScanBoundExceeded` (`discord_reconcile_scan_exhausted`) for
inspection. Uncertain reads preserve the cursor and any existing first strike;
they do not create or erase a strike. A later progressing pass resets it.
Budget exhaustion is not proof of a missing marker or a rejected POST; no
automatic quarantine reset or resend is authorized. See the
[store protocol](audit-store.md) for the locked owner transition, additive
migration and rollback with the column retained.

Only a validated page can prove a marker, exhaustion or a crossed boundary.
A repeated full page, or any page with IDs equal to/newer than the requested
cursor, ends the pass as ambiguous immediately: it cannot renew an identical
cursor indefinitely. Invalid evidence preserves the saved boundary, nonce,
attempt and any accepted receipt; it never produces `MarkerMissing` or clears
prior ambiguity. Existing claim/lease fences still exclude stale owners.
Between full pages, renewal refuses a Reconcile owner whose acceptance was
recorded concurrently, stopping before another read without changing that
receipt or lease. The accepted owner can still complete. A later healthy
owner can reconcile without resending. Valid empty/short pages still prove
exhaustion, and valid full pages continue strictly backwards.

## Kill switch

The service reads `delivery_halt` fresh and feeds `KillSwitchSnapshot`:
a read failure follows legacy fail-open logging (`KillSwitchLog::ReadFailed`).
This does not bypass required durable writes or the final transactional
send authorization: their failures stop delivery. Engage/disengage edges
are logged once each.

## Runtime

`crates/bot/src/audit_runtime.rs` builds one `AuditMirrorService` per
process over the jobs pool and the shared `ActionExecutor`, so mirror posts
use the same pacing lanes as every other REST call. The pool and the bot's
own user id (`GET /users/@me`, the reconciliation author) are resolved on
the first sweep or record call, never at boot; a failed connect caches
nothing and the next sweep retries.

| Variable | Effect |
|---|---|
| `DISCORD_AUDIT_LOG_CHANNEL_ID` | Audit destination; voice and moderation fall back to it |
| `DISCORD_VOICE_LOG_CHANNEL_ID` | Voice destination |
| `DISCORD_MODERATION_LOG_CHANNEL_ID` | Moderation destination |

With none set (blank counts as unset) the runtime is inert: no job, no
handle, and `audit_retry` reports parked with reason `disabled`. A value
that is not a canonical nonzero snowflake parks it as `invalid_config`;
the raw value is never logged.

- `audit_retry` (legacy `audit.retryPending()` + 30 s sweep): every 30 s,
  first run within 5 s of start, timeout 25 s. One `drain_pending_until` call
  claims at most 25 rows. At sweep entry it sets one absolute history deadline
  23 s later, reserving 2 s for checkpoint/release before supervisor cancellation.
  Each Reconcile scan uses the earlier local or shared deadline; later rows
  never receive a fresh sweep budget, and no new claims start once it expires.
  A cancelled page cannot advance the cursor. A DB failure can still prevent
  checkpointing, preserving the original send fence; an in-flight Send keeps
  its existing delivery protocol. An engaged halt skips the sweep before any claim;
  a halt that lands mid-sweep is honored per row and held claims are
  released unattempted. Rows whose store write failed fail the run
  (`database`); every per-row Discord outcome counts as success. Outcomes
  reach `/metrics` through the supervisor's `two_bot_job_*{job="audit_retry"}`
  series, and the halt itself through the label-free
  `two_bot_audit_delivery_halt` gauge (`1` engaged, `0` cleared; an
  unreadable halt keeps the last reported state).
- `audit_runtime::handle()` exposes the same runtime to recorders;
  `AuditRuntime::record` routes and stores one event for the next sweep.
- Logs carry counts and entry IDs only: `audit_retry_swept` (per-outcome
  counts), `audit_entry_quarantined` (entry ID + reason),
  `audit_entry_store_failed` (entry ID, no error text), halt edges.

## Testing

- `crates/discord/tests/audit_mirror.rs`: enforced nonce/mention wire shape,
  status/timeout classification, strict parsing, malformed full pages,
  shared POST/read pacing, authorization after pacing, and concurrent
  slow authorization without overtaking.
- `crates/core/tests/audit_service.rs`: agent-testdb fault injection for
  accepted-post ack loss, adoption ack loss + restart, definite rejection,
  ambiguous history + healthy recovery, privacy/guild/refusal, expired
  prepared senders (including replacement quarantine), and halt/recovery.
- `crates/discord/tests/support/audit_history_evidence.rs`: protected HTTP
  fixtures for complete-page identity, order, size and cursor validation.
- `crates/core/tests/audit_history_evidence.rs`: protected disposable-DB
  acceptance for invalid preflight (zero POSTs), invalid recovery and strict
  progress, exact evidence preservation, stale-owner fences and healthy
  no-resend recovery, plus valid pagination/exhaustion/author/halt/refusal controls.
  This non-ignored target runs in the existing DB-enabled integration lane;
  without `TWO_TEST_DATABASE_URL` it reports a skip, not service evidence.
- `crates/bot/src/audit_runtime_tests.rs`: env gating, once-only delivery,
  halt skip + held-claim release, ambiguous-post reconciliation, the 25-row
  cap, one loopback-REST pass through `ActionExecutor`, job metrics and log
  hygiene (agent-testdb/CI service plus a mirror double; runs in the
  unit/binary `cargo test` step).
- `.github/workflows/check.yml` explicitly runs the ignored `audit_service`
  and `audit_store` tests against the ephemeral CI Postgres service.

On the persistent controller, set only the authorized disposable bootstrap
from [Contributing](../CONTRIBUTING.md#database-tests), then use the bounded
cache wrapper (no direct compiling Cargo or fallback on admission refusal):

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db --test audit_history_evidence -- --test-threads=1
python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db --test audit_store --test audit_service -- --ignored --test-threads=1
python3 scripts/cargo_cache.py run -- test -p two-bot-discord --test audit_mirror
```

Ephemeral CI uses the equivalent locked Cargo commands with its disposable
Postgres service. No live Discord or production/staging database is needed.
