# V7c `/channelinfo` resolution core integration seam

`two_bot_core::voice_channelinfo` is an original, pure implementation derived
only from [the approved voice-room specification](voice-rooms.md#v7-template-admin-aliases-nicknames-inspection).
It requires no `db` feature, Discord wire types, clock, or external I/O.

## Inputs and decisions

- `ChannelInfoContext`: the room's aggregated facts (`RoomContext` with the
  already-resolved owner display name and game title), the V3 `private`/`locked`
  flags, and the room's configured `name_template` plus optional
  `status_template` verbatim. Callers pass headcounts, never rosters: no member
  names, presence details or IDs beyond what `voice_naming::render` already
  exposes travel with it.
- `resolve_variables` returns a `VariableMap` with exactly `VARIABLE_COUNT`
  entries in "All variables" display order: every numbering/people/game/
  stream/party/time/random token from the V5/V6 scope, then derived room state
  (`FULL`, `PRIVATE`, `LOCKED`, `occupant_bucket`, `room_number`). Token values
  reuse the naming engine's own substitution pass, so they match what the room
  name actually shows; they are raw substitutions (no trim, truncation or
  fallback), so empty states surface as empty strings (`@@slots@@` is blank
  when unlimited, `@@stream_name@@` when nobody is live).
- `FULL` needs a limit (`user_limit != 0 && member_count >= user_limit`,
  mirroring V6). `PRIVATE` is always false on standalone channels (mirroring
  V6) and otherwise follows the room's privacy flag. `LOCKED` is the opaque V3
  lock flag the runtime sets when the limit was applied as a headcount lock.
  `occupant_bucket` is the privacy-preserving headcount class
  (`empty`/`solo`/`duo`/`group`).
- `preview_states` renders the room's configured templates under the six
  canonical states in button order (`solo-no-game`, `in-game`, `full`,
  `locked`, `private`, `streaming`), reusing `voice_naming::render` so the V5
  fallback contract holds: previews are never empty and never over 100
  characters. Each state yields a name-template preview plus a status-template
  preview when one is configured; each preview names the state and carries the
  template it rendered. Room identity (number, owner, seed, clock, named lists,
  fallback, channel kind) is preserved so random picks stay stable; only the
  facts that define each state change. Privacy and lock travel in the state
  label: V5 has no privacy or lock token, so `locked` renders the same
  at-limit headcount as `full`, and `private` renders the current facts
  unchanged so the panel can state the name is unaffected.
- `may_inspect(viewer_id, viewer_is_admin, owner_id)`: the room owner may always
  inspect their own room, admins may inspect any room, everyone else is refused.
  Pure ID comparison; zero IDs are never authorized.

## Replay and concurrency contract

The same inputs always produce the same variable map and previews. This core
never mutates state and performs no I/O. No Discord effects or crash recovery
are claimed by these tests.

## Residual parent work

Nickname resolution (`/nick`) and alias storage belong to V7a; command access
gates belong to V10b; the Discord panel (buttons, admin routing, ephemeral
replies) belongs to TOG-10113. The parent must resolve the owner display name
through the V7a nick core and the game title through aliases before calling
this core, authenticate the caller's admin fact, and translate the gate into an
ephemeral refusal. Unit tests establish domain behavior only, not runtime
parity or staging readiness.

## Hermetic verification

```sh
cargo fmt --all -- --check
cargo clippy -p two-bot-core --all-targets --locked -- -D warnings
cargo test -p two-bot-core --test voice_channelinfo --locked
```

The acceptance fixture covers every variable's current value (including empty
states surfacing as empty strings), the V6 `FULL`/`PRIVATE` rules, the lock
flag, all occupant buckets, all six preview states in button order with
name/status template kinds, the no-status single-preview shape, the
full/locked at-limit rendering, the owner-or-admin gate (own vs other ×
admin/owner/stranger, zero IDs refused), and a 5,000-case property test
proving every preview renders under the V5 fallback contract. No tests in this
fixture use a database, Redis, Discord, or a staging identity.
