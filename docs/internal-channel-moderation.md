# Internal channel moderation executor

`two_bot_discord::internal_channel_moderation` (feature `db`) executes
`moderation.purge`, `moderation.slowmode`, `moderation.lockdown` and
`moderation.unlock`. It is a library seam, not an HTTP receiver or slash-command
registration. Member moderation is separate.

## Runtime contract

The receiver must authorize the internal request (HMAC, allowlist, nonce/replay
rules), then parse its body with `InternalChannelRequest::from_body`. Resolve the
requested `actor_id` from the **configured guild**, using actual member roles and
permissions; never construct `ModerationActor` from body-supplied permissions.
Pass server-generated request ID and timestamp, and the authorized idempotency
key, to `InternalChannelExecutor::execute`.

`InternalChannelConfig::enabled` must combine **both** `TWO_MODERATION` and
`TWO_INTERNAL_ALLOW_MODERATION`. The executor checks enablement, resolved actor
identity, and the existing moderation policy before claiming a request or making
REST calls. It retains legacy ordered request hashing, successful replay response
shape, and plain Discord actor attribution in `moderation_audit`. The audit's
human reason remains unsigned, trimmed, and complete. Only the human suffix of
the signed Discord wire reason is shortened to fit 512 UTF-16 units; the entire
`core::mac` marker survives.

Intentional validation tightening: IDs must be canonical nonzero u64 snowflakes;
REST must return the requested channel ID, configured guild ID, and text or
announcement channel type (0 or 5). Foreign-guild/malformed channels and missing
lockdown recovery state never cause a permission mutation.

## Shared ledger and recovery

Run the embedded migrations, including `0123_channel_execution_fence.sql`.
The existing `(guild_id, idempotency_key)` claim token controls replay. The new
`moderation_channel_executions` row additionally serializes different keys on the
same channel, across independent pools/workers. Future slash-command wiring must
use this reservation/settlement protocol too; the low-level REST executor's
unrecorded unlock fallback is **not** a durable unlock path.

Lockdown stores the original exact `@everyone` allow/deny masks before REST and
changes only `SEND_MESSAGES`. Repeated lockdown preserves the first seed and its
recovery generation. Unlock restores those masks, or deletes the overwrite if
none originally existed. Settlement checks both request claim and recovery
generation. Successful result/audit persistence, optional recovery deletion,
and channel release happen in one transaction.

Proven pre-mutation failures release the request/channel reservation atomically
with a refused audit. Channel/history GET failures (including timeouts and rate
limits) and read-only recovery lookup errors abort before any Discord mutation;
recovery-read errors preserve the original seed. A definitive recovery-write SQL
rejection also releases reservations without deleting recovery state: SQLSTATE
classes 22/23/42, serialization failure, deadlock and query cancellation prove the
statement aborted. Unknown completion (including 40003), transport failures and
unrecognized SQLSTATEs remain fenced because a seed may have committed.
Purge's history and deletion phases are separate so history failures are retryable,
not uncertain deletions. The entire history body must be a JSON array of rows with
canonical nonzero u64 string IDs; unreadable bodies or any malformed ID reject the
whole read before deletion or success persistence. A valid empty array succeeds
with zero affected messages.
Rejected first lockdown also removes only its newly created seed; rejected
repeated lockdown keeps the original seed. Build-time validation and confirmed
HTTP 400/401/403/404/405 rejections are safe; per-verb accepted statuses still
complete normally. Uncertain mutation results (unexpected 2xx/3xx, HTTP 408 or
other ambiguous statuses, timeouts, transport errors, rate limits, 5xx),
cancellation, or failed post-mutation database settlement keep the channel/request
fences. Later same-key and distinct-key requests are refused as in progress,
rather than repeating a possibly accepted effect.

There is deliberately **no expiry or automatic reconciliation**: a delayed unlock
must never overwrite a newer lockdown. Recovery requires separately authorized
reconciliation that proves the old REST effect has settled and inspects the
matching claim/channel/recovery generations. The local operator-only
[`moderation release-channel` CLI](channel-lane-reconciliation.md) provides
inspection-first, explicitly confirmed release with generation fencing, atomic
audit and a terminal replay tombstone. It preserves the recovery seed, never
calls Discord, and requires the operator to quiesce old workers and reconcile
actual overwrites/slowmode first. No force-release HTTP endpoint, automatic
reconciliation or live-guild deployment is supplied.

## Verification

The integration suite uses scripted mock Discord REST and a unique schema on the
empty-password disposable `agent-testdb` container. It refuses credential-bearing
or non-test URLs and bypasses pgpass. CI additionally permits the same service on
loopback only when `GITHUB_ACTIONS=true`.

```sh
TWO_TEST_DATABASE_URL=postgres://agent_test@agent-testdb:5432/agent_test \
  python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db \
  --test internal_channel_moderation -- --include-ignored
```

Coverage includes all four actions and replay, legacy actor attribution, signed
Unicode reason bounds, permission/guild refusals, exact mask/absence restoration,
both overlap directions across independent pools, uncertain/rejected writes,
stale request/recovery generations, and atomic rollback after a terminal audit
failure. Regressions cover ambiguous PUT/DELETE permission responses, fault-injected
recovery reads with same-key retry, rejected first/repeated recovery writes,
SQLSTATE uncertainty classification, purge history 503/429/408/timeout and malformed
body/ID recovery, valid empty history, and single/bulk-delete uncertainty without
weakening same-key/distinct-key fences.
CI runs the suite against its existing disposable service container.
