# Channel lockdown/unlock planner acceptance

Card: TOG-12701. Planner: `two_bot_core::channel_moderation` (spec:
`docs/internal-channel-moderation.md`; parity §1 #8–#11). Handler wiring
stays TOG-10174.

## What this pins

1. `validate_purge_count` enforces `1..=100` inclusive; `None`, `0`,
   `101+` refuse with `count` bounds `1..=100`.
2. `validate_slowmode_seconds` enforces `0..=21600` inclusive (`0`
   disables); `None`, `21601+` refuse with `seconds` bounds `0..=21600`.
3. `require_channel_reason` (shared moderation rule) refuses empty/blank
   reasons and trims valid input.
4. `plan_lockdown` moves only `SEND_MESSAGES` (2048): clears it from
   allow, sets it in deny, preserves all unrelated bits, and records the
   exact prior masks as the unlock seed (`None` → absent seed `"0"/"0"`).
5. `plan_unlock` restores the recorded seed verbatim (absent seed →
   `DeleteOverwrite`) and refuses `None` with `NotLocked` — no guessing,
   no unrelated-deny clearing.
6. `moderation_result_text` returns fixed outcome strings
   (`purged (N).` / `slowmode_updated.` / `locked_down.` / `unlocked.`)
   without echoing user input.

## Acceptance

`crates/core/tests/channel_lockdown_acceptance.rs` pins each row above
through the existing public `channel_moderation` API only — no
handler/store/gateway wiring. Synthetic fixtures only.

Run:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test channel_lockdown_acceptance
```
