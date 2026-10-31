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
  (`crates/bot/src/voice_name_panel.rs:394-401`).
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

## Creation receipts and uncertain outcomes

The room, text-companion and Join HTTP adapters accept a successful create body
only after checking it against the request, before returning a channel to the
worker:

- A channel id must deserialize as a nonzero Discord id. Missing, malformed or
  zero channel ids are not receipts.
- `guild_id` must be present, nonzero and equal the requested guild.
- `type` must be guild voice for rooms and Join channels, or guild text for
  companions. Missing, malformed or different kinds are refused.
- `parent_id` must match the requested parent exactly. Companions always request
  their category. Rooms and Join channels may request no parent: an omitted or
  `null` parent is then valid, but an unexpected category is not. A requested
  parent may not be missing, `null`, malformed, zero or different.

A bodyless, malformed or contradictory **2xx is `RoomHttpError::UnknownOutcome`**,
not `InvalidRequest` or a definitive Discord rejection. The POST may have created
something even though its response cannot establish which channel was requested.
The adapters make one POST per invocation, return no channel on rejection, and
never retry the create or expose the remote body in errors. They do not validate
returned overwrite contents or establish provenance for snapshot adoption.

No room binding, creator move, persisted ownership or deletion authority may be
derived from a rejected receipt. The room worker drops the pending create and
keeps its durable reservation held: it neither reposts nor settles/refunds that
reservation on `UnknownOutcome`. Elapsed time and an invalid receipt do not prove
channel absence. Existing reconciliation/release holds remain in force; this
change implements no operator release path and grants no new create/delete
permission.

For a companion, the worker may still adopt a channel from its separate live
snapshot on an uncertain result. That is not acceptance of the rejected receipt;
when no snapshot candidate exists, that dispatch persists/tracks nothing and
backs off. Existing later queue retry/adoption behavior is unchanged here.

**Known separate Join retry/adoption gap:** `dispatch_create_join` records an
uncertain failure and leaves the Join merely requested, without a stored channel
id or `join_deletable` entry. A later queue attempt tries snapshot adoption by
kind, category, name, relative id and absence of another tracked owner. If it
finds no candidate, it can fall through to another POST; on the final uncertain
attempt it forgets the plan, allowing a future `/private` to plan again. Those
heuristics do not prove creation provenance or provide a durable uncertainty
fence. Receipt validation does not fix that retry/adoption design, promise
at-most-once Join creation across dispatches, or authorize deleting an uncertain
channel. It only prevents ownership/deletion authority from the rejected body.

Loopback adapter tests cover the three receipt matrices and one-POST boundary;
bot worker negative controls pin no bind/move/persist/delete/refund from rejected
evidence. They use in-memory fakes, not a live guild or database. A merged change
is not evidence of production installation or a live Discord creation.

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
