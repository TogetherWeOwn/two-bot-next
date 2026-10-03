# V10b room-command access gate

`two_bot_core::voice_access` is an original, pure implementation derived only
from [the voice-room specification](voice-rooms.md#v10-logging-health-errors-utilities).
It needs no `db` feature, Discord types, clock, or I/O.

## Inputs and decisions

- `AccessControls`: `room_creation_enabled`, an optional guild-wide
  `required_role`, and a per-command map from command name to allowed roles.
  All IDs are plain `u64` (`RoleId`); the runtime authenticates identity,
  evaluates Manage Channels into the `is_admin` fact, and persists settings.
- `AccessMember`: `is_admin` plus the member's role IDs.
- `may_use_command` returns `Allow` or `Deny(reason)`, evaluated in order:
  admin first (always allowed), then `Deny(RequiredRole)` when the member
  lacks the guild-wide role, then `Deny(CommandRestricted)` when a restricted
  command has no matching role, otherwise `Allow`.
- `may_create_room` is the global creation kill-switch. When off, the runtime
  must not create rooms from creator joins; commands on existing rooms keep
  working through `may_use_command`. Admins do not bypass the switch: it
  stops creation for the guild, not for a member.
- `VOICE_COMMANDS` lists every restrictable slash command (no leading slash),
  taken from the spec. `validate_access_controls` refuses unknown restriction
  keys (exact lowercase match) and zero role IDs.

A restriction entry with an **empty role list denies every non-admin**
(fail closed). Delete the entry to lift the restriction; clearing
`required_role` lifts the guild-wide gate. Unknown invoked names match no
entry, so only the guild-wide gate applies — the runtime must validate
restriction keys before they become silent no-ops.

## Replay and concurrency contract

Decisions are pure functions of the supplied facts: the same inputs always
produce the same outcome, and refusals change nothing. The runtime must
serialize per-guild settings updates, persist validated controls
atomically, and deduplicate interaction IDs. Stale snapshots and delayed
event replays are not detected by this core.

## Persistence

`PgRoomStore::access_controls(guild)` / `save_access_controls(guild, controls)`
(migration `0227_voice_access_controls.sql`, table `voice_access_controls`)
read and replace the whole per-guild row in one upsert. No row reads as
`AccessControls::default()` (creation on, nothing restricted). Saves run
`validate_access_controls` first, and loads re-validate, so a hand-edited row
with an unknown command or zero role fails closed instead of becoming a
silent no-op. The runtime still owns per-guild serialization and enforcement.

## Residual parent work

Room lifecycle (V1), owner controls (V2/V3), logging/health/error routing
(V10a), permission evaluation, ephemeral replies, and runtime wiring remain
outside this slice.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- check -p two-bot-core
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_access
```

The acceptance fixture covers every decision branch, the empty-list rule,
creation-off command behavior, unknown-command validation, and property
tests for admin-never-denied and restriction-removal monotonicity. No
network, Discord, database, or staging identity is used.
