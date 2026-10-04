# `/name` panel, custom-name modal and restore button

Runtime wiring for the V3 `/name` control in `docs/voice-rooms.md`. The
decisions are pure and live in `two_bot_core::voice_room_name`; the worker,
the persistence and the Discord responses live in
`crates/bot/src/voice_name_panel.rs`.

## Flow

1. `/name` replies (ephemeral) with a panel for the temporary room the invoker
   is in: its current name, whether a custom name is set, and two buttons.
2. **Custom name** opens a modal, pre-filled with the current custom text.
   Submitting it sets the override. Template tokens are allowed.
3. **Restore template** clears the override and renames the room to its
   template name.

Every component id is the request-bound `two:voice:name-custom|name-restore|
name-modal:<room_id>` id from `two_bot_core::voice_custom_id`, parsed back as
untrusted input. A modal must be the initial callback, so the Custom name
click answers with the modal directly; every other step defers and then edits
its ephemeral reply.

## Who may use it

The room's current owner, or a server admin (Manage Channels). The check runs
in the guild worker on every step, against the room's owner at that moment,
so a stale panel from before a `/transfer` or a caretaker handoff is refused.
The guild role gate (`/access restrict name ...`) applies to the slash command
and to each click and submit. A room id that is not tracked (deleted, or never
a room) is answered with "no longer exists" and changes nothing.

## What a custom name is

The stored override is the trimmed text as typed, at most 100 characters,
template syntax intact. It is checked in this order (`decide_custom_name`):

1. Non-empty and within 100 characters.
2. The template text passes the room-name sanitizer and automod name filter
   (`voice_name_filter`), so a blocked word in a conditional branch that is not
   active yet is refused now.
3. It renders through the full V6 template engine against the room's live facts
   (`voice_template::resolve_room_name`) and the render passes the same filter,
   because tokens such as `@@owner@@` pull in member-controlled text.
4. When the guild's "unique names" setting is on **and the text is a literal
   name** (no tokens, numbering, plurals, choices or conditionals), the folded
   name (NFKC, lowercase, trim) must not match another voice channel. The room
   itself is excluded. A name built from tokens follows the room, so it is not
   compared.

Refusals are ephemeral and never echo the rejected name.

Rendering facts come from the live guild snapshot: owner and creator display
names (from the gateway cache), headcount, whether the owner is present, the
channel's user limit, the room's stored seed, the guild's named lists and
"no game" label, and the current time (UTC). Presence data (games, streams)
and privacy are not tracked by this runtime, so tokens built from them render
in their "nothing to show" state. `##` numbers a room by its position among
its creator's tracked rooms until room numbers are persisted.

## Restore

`decide_template_name` renders the creator channel's name template against the
room, or `{owner}'s room` (the name the create path gives a room) when the
template is blank, and runs it through the same sanitizer and filter. A
template the filter blocks refuses the restore and keeps the override. A
display name the filter blocks is replaced by "member" in the fallback.

## Storage and the rename

The override lives in `voice_rooms.custom_name` (migration 0414), `NULL` for a
room on its template name, with `name_touched_at` stamped on every set or
restore so the rollback delta measures a renamed room. The worker keeps the
override in memory, persists it through the urgent `SetCustomName` queue
action (retried on failure, stale writes skipped), and hands the rendered
name to the existing rename coalescer, which keeps one pending name per
channel and releases it no faster than Discord's rename limit. Ownership
changes keep the override; deleting the room drops it with the row. Member
erasure deletes whole `voice_rooms` rows, so it removes the override too.

## Not covered here

Re-rendering a custom name when occupancy, activity or the limit changes
(the V5 recalculation) is not wired into this runtime yet; the name is
rendered when the owner sets it and when the room is restored. Publishing the
`name` command is the registry publish change, behind `TWO_VOICE`.
