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
  the join. `/group` is wired: when the creator's `group_by_category` is set, the
  worker reads it (`crates/bot/src/voice_rooms.rs:3427-3444`) and forwards
  `grouped`/`group_room_ids` into `plan_placement`
  (`crates/bot/src/voice_room_plan.rs:206-212`). Re-checked at `86a6668a7`;
  name-display numbering still ranks per creator, not per category
  (`crates/bot/src/voice_name_panel.rs:394-401).
- **Permission inheritance.** The creator's stored source (creator channel,
  category, or a chosen channel) supplies the overrides. Every emitted allow
  excludes Manage Roles, including inherited role/member, owner and bot
  targets. Source denies (including Manage Roles) are retained; deny still
  wins on a target. The owner gets the V8a owner grant on their own room only;
  private rooms deny Connect to @everyone. A missing chosen source channel is
  refused.
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

The per-creator settings commands (`/position`, `/group`,
`/inheritpermissions`, `/defaultlimit`, `/alwaysprivate`) are wired as
admin-gated slash commands with single-field writes, and the required-role
setting is wired too: `/access role` parses (`crates/bot/src/voice_rooms.rs:6980`),
applies `AccessAction::RequiredRole` (`:7599`), and the gate refuses callers
without the role (`:7224-7227`; core check
`crates/core/src/voice_access.rs:127-162`). Re-checked at `86a6668a7`.
