# Channel lockdown/unlock acceptance

Planner: `two_bot_core::channel_moderation`; runtime contract:
[`internal-channel-moderation.md`](internal-channel-moderation.md).

## What this pins

1. `validate_purge_count` enforces `1..=100` inclusive; missing or out-of-range
   values refuse with `count` bounds `1..=100`.
2. `validate_slowmode_seconds` enforces `0..=21600` inclusive (`0` disables);
   missing or out-of-range values refuse with `seconds` bounds `0..=21600`.
3. `require_channel_reason` refuses empty/blank reasons and trims valid input.
4. `plan_lockdown` moves only `SEND_MESSAGES` (2048): clears it from allow,
   sets it in deny, preserves unrelated bits, and records the live prior masks
   (`None` → absent seed `"0"/"0"`). Repeated lockdown preserves the seed while
   the live deny remains set; otherwise the store refreshes seed and generation.
5. `plan_unlock` requires recorded state **and a fresh live overwrite**. It
   restores only the recorded `SEND_MESSAGES` allow/deny bits, keeping every
   other live bit. A prior send deny stays denied. It refuses an untracked
   channel, unreadable masks, a missing live entry, or send-bit drift (send
   newly allowed or no longer denied). The runtime gives a clear refusal and
   keeps recovery state; it does not silently restore the old snapshot.
6. An originally absent overwrite becomes `DeleteOverwrite` only if the planned
   masks are both zero. New permissions added during lockdown require a PUT,
   not deletion of the whole overwrite.
7. `moderation_result_text` returns fixed outcome strings without echoing input.

## Safety regression

Lock a channel, then have an administrator deny `VIEW_CHANNEL` on `@everyone`.
Unlock must preserve that live deny, even if the stored seed allowed viewing or
had no overwrite at all. Similarly, a manually removed send deny ends the old
cycle: the next lockdown must not resurrect its old send allow on unlock.

GET and PUT/DELETE are separate Discord requests, not a conditional transaction.
External edits between them are not fenced by the bot's channel reservation.
This is not a guarantee against simultaneous administrative writes; coordinate
manual overwrite edits with bot commands. No live guild or production test is
required by the synthetic regressions below.

## Verification

`crates/core/tests/channel_lockdown_acceptance.rs` pins the planner through its
public API, without Discord or a database. The store suite pins seed refresh and
stale-generation cleanup. The slash and internal-channel runtime suites use mock
REST plus the explicitly guarded disposable test service, never staging or
production. They verify GET-before-write, preservation of live privacy changes,
drift/read-failure refusal, replay, and recovery retention on uncertain writes.

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test channel_lockdown_acceptance
TWO_TEST_DATABASE_URL=postgres://agent_test@agent-testdb:5432/agent_test \
  python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db --lib channel_moderation -- --include-ignored
TWO_TEST_DATABASE_URL=postgres://agent_test@agent-testdb:5432/agent_test \
  python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db \
  --test channel_moderation_runtime --test internal_channel_moderation -- --include-ignored
```
