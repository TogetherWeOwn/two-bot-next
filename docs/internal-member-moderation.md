# Internal member moderation executor

`two_bot_discord::internal_member_moderation` (feature `db`) executes
`moderation.ban`, `moderation.tempban`, `moderation.kick`, `moderation.timeout`
and `moderation.warn`. It is a library seam, not an HTTP receiver or
slash-command registration. Channel moderation is separate.

## Runtime contract

The receiver must authorize the internal request (HMAC, allowlist, nonce/replay
rules), then parse its body with `InternalMemberRequest::from_body`. Resolve
both the requested `actor_id` and the target `discord_id` from the **configured
guild**, using actual member roles, positions, permissions, bot and ownership
flags; never construct `ModerationActor` or `ModerationTarget` from
body-supplied roles or permissions. Pass the runtime's read of the bot's own
highest role position, a server-generated request ID, the authorized
idempotency key, and the caller's clock reading in unix millis to
`InternalMemberExecutor::execute`.

`InternalMemberConfig::enabled` must combine **both** `TWO_MODERATION` and
`TWO_INTERNAL_ALLOW_MODERATION`. The executor checks enablement and resolved
actor/target identity before claiming a request or making REST calls, then runs
the shared `MemberModerationService`: hierarchy, protected-role and permission
policy, idempotency claim before Discord, prepared-ban fencing, tempban
expiry scheduling, and the `moderation_audit` ledger are all service-owned, so
ledger rows are identical whether the action came from a slash command or the
website. The audit's human reason remains unsigned, trimmed, and complete. Only
the Discord wire reason carries the `core::mac` marker, bound to guild,
idempotency key, action and actor; only its human suffix shortens to fit 512
UTF-16 units, and the entire marker survives.

Intentional validation tightening: IDs must be canonical nonzero u64
snowflakes; tempban needs 60–365-day `duration_seconds`, timeout 60–28-day,
both required. Local guard refusals never reached the wire, so like confirmed
4xx they release the claim for a real second attempt; timeouts, rate limits,
5xx and transport failures stay fenced as `in_progress`, never auto-retried.

## Shared ledger and recovery

No new tables: the existing `moderation_idempotency`, `moderation_audit`,
`moderation_member_bans` and `moderation_scheduled_unbans` rows (migrations
`0110`–`0113`) carry both entry paths. Future slash-command wiring must drive
the same `MemberModerationService`; nothing here duplicates its claim, audit
or expiry protocol. The unban sweep itself stays with the service slice — this
module only schedules tempban expiries through it.

## Verification

Unit tests run over the in-memory member store with scripted mock Discord
REST: all five verbs, exact Discord routes, hierarchy and protected-role
refusals with no claim or wire call, tempban scheduling through sweep
dispatch, idempotent replay, key-mismatch refusal, rejection release for
retry, uncertain-timeout fencing, and a parity case proving the website path
writes byte-identical audit rows to the direct service path.

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db \
  --test internal_member_moderation
```

The disposable-DB acceptance lane repeats all five verbs plus replay, exact
tempban expiry, and refusal writes-nothing against `agent-testdb` or the CI
service container. It refuses credential-bearing or non-test URLs.

```sh
TWO_TEST_DATABASE_URL=postgres://agent_test@agent-testdb:5432/agent_test \
  python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db \
  --test internal_member_moderation -- --include-ignored
```

CI runs the suite against its existing disposable service container.
