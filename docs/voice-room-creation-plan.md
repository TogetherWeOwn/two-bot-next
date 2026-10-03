# V8 room creation plan (runtime wiring)

`crates/bot/src/voice_room_plan.rs` joins the pure V8 cores
([placement](voice-placement-core.md), [permissions](voice-permissions-core.md))
to the live guild snapshot. The guild worker calls `plan_room` once per join and
passes the result straight to the single channel-create request. Overrides are
included in that request and never patched afterwards.

## Behaviour

- **Placement.** The new room takes the slot directly above or below the creator
  (the creator's stored `/position`), computed from the creator's category in
  `(position, id)` order. Existing channels are never moved. The index becomes
  Discord's create-time `position` via `position_for_index`. A category that
  cannot be planned leaves the position to Discord (append) instead of blocking
  the join. `/group` is not wired yet (see below).
- **Permission inheritance.** The creator's stored source (creator channel,
  category, or a chosen channel) supplies the overrides that are copied. The
  owner gets the V8a owner grant on their own room only; private rooms deny
  Connect to @everyone. A missing chosen source channel is refused.
- **No Manage Roles.** The bot cannot set overrides, so none are sent and the
  room is created inside the category, which syncs it. A creator whose default
  is private is refused (`AccessDenied`) instead of producing a public room.
- **Bot access in private rooms.** @everyone's Connect deny also removes the
  bot's guild-level Connect, which it needs to move the owner in. Only when the
  planned overrides leave the bot unable to manage the room, it adds a member
  override for itself with View, Connect, Manage Channels and Move Members. If
  that still is not enough (an explicit deny on the bot), the create is refused.
- **Per-creator defaults.** `default_limit` (else the creator channel's limit)
  and `private_default` set only the new room's starting state; existing rooms
  keep theirs. The companion text channel (`text_channels`) does not change the
  voice room's plan: the worker creates it through the same per-guild queue
  (V9c), after the room exists.

## Assumptions to verify on staging

Discord does not document how create-time `position` interacts with sibling
positions. `position_for_index` asks for the slot of the channel the new room
displaces, assuming Discord inserts there. A staging run (V12) should confirm the
room lands next to its creator; if not, only that function changes.

## Not in this slice

`/group` (shared numbering and a contiguous block) needs a stored per-creator
flag and a migration; the settings commands (`/position`, `/inheritpermissions`,
`/defaultlimit`, `/alwaysprivate`) and the required-role setting are not wired.
Until then the stored defaults come from `/create` (spec defaults) or the V11
import.
