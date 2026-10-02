# V8a permission-override builder core integration seam

`two_bot_core::voice_permissions` is an original, pure implementation derived
only from [the approved voice-room specification](voice-rooms.md#v8-placement-permissions-per-creator-defaults).
It requires no `db` feature, Discord wire types, clock, or external I/O.

## Inputs and decisions

- `RoomPermissionInput`: the inheritance source (`CreatorChannel`, `Category`,
  or `ChosenChannel`) with that source's override list, `bot_can_manage_roles`,
  `owner_id`, `owner_extra_allow`, `private` (rooms in this slice are
  always-private), `everyone_role_id`, and an optional `required_role`. All
  IDs are caller-authenticated facts; guild routing and the actual channel
  create belong to the runtime.
- Permissions are plain `u64` bitfields with `PERM_*` constants matching
  Discord's bit layout for the touched bits. Untouched bits pass through
  inside copied source overrides.
- `plan_room_overrides` returns `SyncToCategory` when the bot lacks Manage
  Roles (it cannot legally set any override), otherwise `Overrides(list)`:
  the complete override set to include in the channel-create call, never
  patched afterwards. At most one entry per `(id, kind)`; every entry has
  `allow & deny == 0` because deny wins over allow on the same target.
- The three inheritance sources behave identically in the builder; the caller
  resolves which channel's overrides to pass, and that choice stays visible
  on the input for audit.
- Private rooms deny Connect to @everyone while preserving the source's View
  allow ("keep View" preserves, never creates; a source View deny is likewise
  preserved). The owner keeps Connect through their own member grant.
- Owner grant, on the owner's own room only: `OWNER_ALLOW_BITS` (View,
  Connect, Speak, Stream, voice activity, priority speaker, Manage Channels,
  mute/deafen/move) plus `owner_extra_allow` clamped to `OWNER_EXTRA_MASK`
  (the base set plus Manage Events). Neither can contain Manage Roles or
  Administrator, so owning a room never escalates privilege. A source deny on
  the owner still wins over the grant.
- In private rooms only, the required role (never @everyone; that is refused)
  receives View/Connect/Speak (`REQUIRED_ROLE_ALLOW_BITS`) so its members
  keep basic access despite the @everyone Connect deny. A source deny still
  wins. Public rooms grant the required role nothing.

## Replay and concurrency contract

The same inputs always produce the same plan. The runtime must pass the
returned list to the channel-create call atomically, persist the room against
that creation, and deduplicate interaction/event IDs. Stale snapshots are not
detected by this pure core. No Discord effects or crash recovery are claimed
by these tests.

## Residual parent work

Room lifecycle/storage, membership tracking, creator-channel settings
(`alwaysprivate`, required role, inheritance choice), permission
authentication, the channel-create/move/delete calls, and the Join-channel
flow remain outside this slice. The V8 parent must route the settings,
authenticate the caller's facts, and consume this module behind its V1 edge.
Unit tests establish domain behavior only, not runtime parity or staging
readiness.

## Hermetic verification

```sh
cargo fmt --all -- --check
cargo clippy -p two-bot-core --all-targets --locked -- -D warnings
cargo test -p two-bot-core --test voice_permissions --locked
```

The acceptance fixture covers the Manage-Roles fallback, verbatim source
copy, private @everyone deny with View/owner-Connect preserved, the exact
documented owner bits with escalation clamped, source-deny-wins on the
owner, required-role grants (private only), source-variant equivalence, and
invalid inputs. A 256-case property test proves the security contract: no
output grants a bit the source did not allow except the documented grants on
their own targets, deny wins everywhere, and no duplicate targets; the owner
entry is checked for its exact expected value including zero Manage
Roles/Administrator bits. No tests in this fixture use a database, Redis,
Discord, or a staging identity.
