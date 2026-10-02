# V9 companion text-channel lifecycle acceptance

Tests-only V9 acceptance for the companion text-channel lifecycle, derived
only from [the approved voice-room specification](voice-rooms.md#v9-temporary-text-channels)
and [the V9 core seam](voice-text-channel-core.md). It pins the existing
`two_bot_core::voice_text_channel` public API and adds no implementation:
no `db` feature, Discord wire types, clock, store, or external I/O.

## What the fixture pins

- `text_channel_plan` visibility matrix: toggle-off plans nothing; a solo
  room is visible only to its occupant; a group room is visible to
  occupants and admins; a configured viewer role gets an overwrite while
  @everyone stays denied; a viewer role of @everyone (the guild ID) makes
  the companion visible to all with no separate role overwrite.
- `sanitise_channel_name`: empty/blank/dash-only names fall back to
  `voice-chat`; oversized names truncate to 100 characters; codepoints with
  no lowercase mapping (e.g. U+1D400) are dropped; ordinary names are
  lowercased with whitespace runs collapsed to `-`.
- `occupancy_diff` transitions: join grants the newcomer, leave revokes the
  departed member, unchanged (or duplicated) occupancy is a no-op, and
  protected viewer/admin IDs are never revoked.
- `plan_companion_deletion`: a deleted room maps to a delete of its
  companion channel.
- Companion deletion never touches the voice room itself: the room facts
  (IDs, occupants, admins) are unchanged and the companion can still be
  planned from them afterwards.

## Hermetic verification

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_companion_lifecycle
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets -- -D warnings
```

No test in this fixture uses a database, Redis, Discord, or a staging
identity. Runtime wiring (persisting the settings snapshot, passing the
viewer role and admins as `protected`, executing the Discord delete)
remains parent work under TOG-10109.
