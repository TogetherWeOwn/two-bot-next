# V3a owner room-controls core

`two_bot_core::voice_room_controls` implements only the pure decisions from
[`voice-rooms.md` §V3](voice-rooms.md#v3-owner-room-controls). No Discord
types, REST, persistence, permission checks, template expansion or
room-lifecycle runtime are introduced.

## Domain contract

- `parse_limit(arg, headcount)`: `Some(0)` is unlimited, `Some(1..=99)` sets
  that limit, `Some(>99)` is `LimitError::OutOfRange`. `None` locks the room
  at the current headcount: headcount `0` (empty room) is unlimited, `1..=99`
  locks at that count, and larger headcounts clamp to `99` so a lock never
  produces a limit above 99. `unlimit()` clears the limit. `RoomLimit`
  converts to the Discord user-limit value (`0` for unlimited).
- Bitrate preferences: `validate_bitrate_preference(value, tier_max)` accepts
  only values strictly above 8 kbps (`>= 8001`) and at most the guild tier
  maximum. `BitrateTier` is a table input (Base 64k, Level 1 128k, Level 2
  256k, Level 3 384k bps) via `tier_max_bps`. `reset_bitrate_preference()`
  (alias `RESET_BITRATE_PREFERENCE`) clears a preference to `None`.
- `room_bitrate(prefs, creator_default, tier_max)`: the average of the set
  (`Some`) preferences, ignoring `None` members; with no preferences it falls
  back to the creator channel bitrate. The average uses integer division and
  rounds down (floor): `[8001, 8002]` averages to `8001`. The sum accumulates
  in `u64` so no input count can overflow. The result, including the fallback,
  is always clamped into `[8001, tier_max]` via `clamp_bitrate`. Set-time
  enforcement belongs to `validate_bitrate_preference`; the average tolerates
  stored out-of-range values through the final clamp.
- `name_conflicts(candidate, existing_voice_names, unique_names_enabled)`:
  literal names only (no template expansion), compared after folding NFKC +
  Unicode lowercase and trimming (legacy `rename.ts` rule), so `"Room"`,
  `"room"` and full-width `"Ｒｏｏｍ"` all conflict. Returns `false`
  whenever the guild "unique names" setting is off. Whether the candidate's
  own channel is in the supplied list is the parent's routing decision
  (exclude it for renames).

## Runtime wiring

`/limit [count]` and `/unlimit` are wired through the room worker
(`crates/bot/src/voice_rooms_limit.rs`). The worker resolves the caller's
current room from live voice state and reuses the `/transfer` owner gate
(`voice_ownership::require_room_owner`): the owner or an admin standing in the
room may act; anyone else, a caller outside voice and a caller in an unmanaged
channel are refused. The human headcount (bots excluded) feeds `parse_limit`,
which gives `0` = unlimited, `1..=99` as set, a refusal above 99, and a lock at
the headcount when no count is given. The decided value is queued as
`RoomAction::SetUserLimit` on the guild's ordered queue, so a 429 returns to the
queue with its retry-after and ten failed attempts dead-letter in the `limit`
family of `two_bot_voice_dead_letters_total`. The reply is ephemeral and waits
briefly for the write: a terminal Discord refusal says so, and a write still
waiting on the queue is reported as queued. Both commands are role-restrictable
through `/access restrict` like every other voice command.

## Residual parent integration (not parity evidence)

The parent still owns bitrate preferences and their persistence, the `/name`
panel with template expansion and the unique-names check, `/private`,
`/public` and the Join-channel wiring. The `/limit` and `/unlimit` definitions
publish through the normal registry sync when the voice gate is on (see above).
No live channel update is performed or verified by this
component's tests.

`/name` is wired: see [`voice-name-panel.md`](voice-name-panel.md). `/private`
and `/public` are wired with the Join channel: see
[`voice-private-core.md`](voice-private-core.md#runtime-wiring-private-and-public).
The join-request buttons are wired too: see
[`voice-private-core.md`](voice-private-core.md#runtime-wiring-join-requests).
`/limit` and `/unlimit` are runtime-wired as described above, defined in
`voice_commands()` (`crates/core/src/voice_rooms.rs:1646-1660`), merged into
the publish set when the voice gate is on (`crates/core/src/router.rs:717-719`),
and synced to the registry on ready
(`crates/bot/src/command_runtime.rs:1009-1020`); the bitrate preference is not
wired. Re-checked at `86a6668a7`.

## Hermetic verification

```sh
cargo fmt --all -- --check
cargo clippy -p two-bot-core --all-targets --locked -- -D warnings
cargo test -p two-bot-core --test voice_room_controls --locked
```

The acceptance fixture covers the full edge table (0, 1, 99, 100, None, 8000,
8001, tier max, tier max + 1), floor rounding, fallback clamping,
folded (NFKC + lowercase + trim) collision matching, the off-switch, and property tests pinning
the bitrate bounds and the lock `<= 99` invariant. No network, Discord,
database, credentials or sleep is needed.
