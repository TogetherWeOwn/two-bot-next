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

## Purge completion and early stops

A completed purge keeps the slash reply `Moderation action completed: purged (N).`
and the existing audit metadata, internal response and stored replay payload byte
for byte. It omits `stop_reason` entirely, rather than adding a null field, and
its outer receipt stores NULL.

If a delete is refused after at least one confirmed deletion, the purge stops
without retrying or resuming. The count still includes only confirmed deletions.
The slash reply is `Moderation action completed: purged (N, stopped early: refused).`
or `Moderation action completed: purged (N, stopped early: rate_limited).` A 429
produces `rate_limited`; other definitive refusals produce `refused`. Both audit
paths and the internal stored result append `stop_reason` last. The signed HTTP
response appends it to `result` after `affected`, for example
`{"outcome":"purged","affected":1,"stop_reason":"rate_limited"}`. The outer
receipt and its terminal audit keep the reason (migration `0448`), so outer and
inner replays return the same reason without another Discord call. The audit
outcome remains `purged`.

A refusal before any deletion still errors; a timeout or unavailable response
after a deletion still retains the uncertain-effect fences rather than claiming
completion. A message already gone (404) does not increment the deleted count.

## Shared ledger and recovery

Run the embedded migrations, including `0123_channel_execution_fence.sql`.
The existing `(guild_id, idempotency_key)` claim token controls replay. The new
`moderation_channel_executions` row additionally serializes different keys on the
same channel, across independent pools/workers. Future slash-command wiring must
use this reservation/settlement protocol too; the low-level REST executor's
unrecorded unlock fallback is **not** a durable unlock path.

Lockdown stores the live `@everyone` allow/deny masks before REST and changes
only the lockdown bits (`SEND_MESSAGES`, `SEND_MESSAGES_IN_THREADS`,
`CREATE_PUBLIC_THREADS`, `CREATE_PRIVATE_THREADS`, `ADD_REACTIONS`). A role or
member overwrite that allows sending still wins. Repeated lockdown preserves the first seed and generation
while the live send deny remains set. If an external edit clears that deny (or
removes the overwrite), the next lockdown atomically refreshes the seed and
recovery generation before PUT.

Unlock reads the live overwrite and restores **only the recorded lockdown
bits** (a thread or reaction bit only while it still holds the locked state).
All other live allow/deny bits survive, including a newly added
`VIEW_CHANNEL` deny. An originally absent overwrite is deleted only when both
result masks are zero; otherwise unlock PUTs the preserved live masks. Missing
recovery, unreadable/missing live state, a cleared send deny, or a newly allowed
send bit refuse without a permission mutation and retain the recovery row.
The refusal explains send-bit drift and asks for reconciliation rather than
restoring a stale snapshot. A pre-existing send deny is not cleared by unlock.
Settlement checks both request claim and recovery generation. Successful
result/audit persistence, optional recovery deletion, and channel release happen
in one transaction.

Discord does not offer a conditional overwrite PUT/DELETE: the fresh GET and
write are not atomic with external admin edits. The channel fence serializes bot
commands, not Discord administrators. Avoid simultaneous manual overwrite edits
and bot unlocks; this change prevents stale stored snapshots from widening access,
not every possible read/write race.

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
Rejected first lockdown (or a refreshed cycle) removes only its new seed;
rejected repeated lockdown with an intact send deny keeps the original seed. Build-time validation and confirmed
HTTP 400/401/403/404/405 rejections are safe; per-verb accepted statuses still
complete normally. A mutation HTTP 429 also proves no effect
(`DiscordError::proves_no_effect`): it releases only the failed attempt's
reservations with a refused audit, retiring only a first lockdown's own new
seed and preserving any prior recovery seed, so a same-key retry is a real
second attempt. Uncertain mutation results (unexpected 2xx/3xx, HTTP 408 or
other ambiguous statuses, timeouts, transport errors, 5xx),
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
weakening same-key/distinct-key fences. Mutation-429 release for single/bulk
delete, overwrite and slowmode — with recovery seeds preserved and 5xx, timeout,
cancellation and unexpected statuses still fenced — is covered by
`crates/discord/tests/mutation_429_proves_no_effect_channel.rs`, run by its own
CI step against the same disposable service.
CI runs the suite against its existing disposable service container.
