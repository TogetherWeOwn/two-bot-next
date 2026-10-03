# V9 companion text-channel core integration seam

`two_bot_core::voice_text_channel` is an original, pure implementation derived only
from [the approved voice-room specification](voice-rooms.md#v9-temporary-text-channels).
It requires no `db` feature, Discord wire types, clock, or external I/O.

## Inputs and plans

- `TextChannelSettings`: the per-creator `/textchannels` toggle (off by default),
  an optional configured name, and an optional viewer role. `viewer_role_id` equal
  to the guild ID means @everyone. The `Default` is toggle-off, which plans nothing.
- `VoiceRoomFacts`: guild, room and category IDs plus current occupants and the
  room's effective admins. Zero IDs are ignored, never emitted. The runtime must
  authenticate admin status; it is a supplied fact, not computed here.
- `text_channel_plan`: returns `None` when the toggle is off. Otherwise the
  companion lives in the room's category with the sanitised name and initial
  overwrites: @everyone denied View (allowed instead when the viewer role is
  @everyone), the viewer role allowed, and every current occupant and admin
  allowed. The plan also carries `settings`, the snapshot taken at creation.
- `VoiceRoomFacts::admin_role_ids` and `admin_view_roles` (V9d): guild roles that
  hold Manage Channels become `Role` View allows in the plan, so an admin
  promoted later is covered without per-room edits. Administrator roles (they
  bypass overwrites), @everyone (already covered) and zero IDs never get an
  entry; a role that is also the viewer role is emitted once. The runtime
  resolves the roles live from its role snapshot at plan time, and passes them
  as `protected` alongside the viewer role and the bot. The `/textchannels`
  command (Manage Channels, a required creator channel plus optional `enabled`,
  `name` and `viewer-role`; `enabled` defaults to on) edits the creator row
  only, so open companions keep their creation-time snapshot.
- `sanitise_channel_name`: lowercase, whitespace runs collapse to one `-` with
  edges trimmed, at most 100 characters; codepoints with no lowercase mapping
  (e.g. U+1D400) are dropped; blank or empty results fall back to
  `voice-chat`. Sanitising is fixed-point.
- `occupancy_diff(before, after, protected)`: grants on join, revokes on leave,
  except `protected` IDs are never revoked. Both lists are deduplicated and
  sorted; unchanged occupancy yields empty lists. The runtime passes the viewer
  role and admins as `protected`, so a protected member who leaves keeps View
  until the runtime decides otherwise. Removing an ID from `protected` does not
  itself revoke it.
- `plan_companion_deletion`: a deleted room maps to a delete of its companion;
  the runtime executes the Discord delete.

## Replay and concurrency contract

The same inputs always produce the same plan or diff. Applying grants then
revokes to `before` yields `after` plus any protected members who left; with no
protected members it yields exactly `after`. Grants and revokes stay disjoint.

A settings change affects only channels created afterwards: the runtime must
store each plan's `settings` snapshot with the companion record and never apply
later settings retroactively. Stale snapshots and delayed event replays are
**not** detected by this pure core.

## Residual parent work

V1 room storage, membership tracking, Discord channel/category reads and writes,
permission authentication, companion persistence and deletion, and runtime wiring
remain outside this slice. The V9 parent (TOG-10109) must route `/textchannels`,
persist the snapshot, pass the viewer role and admins as `protected` on every
join/leave, and delete the companion with its room. Unit tests establish domain
behavior only, not runtime parity or staging readiness.

## Hermetic verification

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_text_channel
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets -- -D warnings
```

The acceptance fixture covers toggle-off default, default/custom/everyone
viewer roles, name sanitisation, join grant/leave revoke, protected retention,
snapshot isolation, companion deletion, plus property tests for diff
application, disjointness/ordering, and name rules. No tests use a database,
Redis, Discord, or a staging identity.
