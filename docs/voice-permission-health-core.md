# V10a permission-health core integration seam

`two_bot_core::voice_permission_health` is an original, pure implementation
derived only from [the approved voice-room specification](voice-rooms.md#v10-logging-health-errors-utilities).
It extends the merged offline contract
([voice-health-contract.md](voice-health-contract.md), which only classifies
failures into sanitized diagnostics) with the remaining pure V10 logic: which
level denies a permission, where the notice goes, and how often it repeats. It
requires no `db` feature, Discord wire types, clock, or external I/O.

## Inputs and decisions

- `resolve_effective_permissions`: standard Discord resolution as a pure
  function — guild base, then @everyone, then combined role overwrites (allow
  wins over deny), then member overwrite. A guild-level Administrator base
  yields all bits. Administrator is not a channel permission, so its bit is
  ignored in overwrite masks instead of hiding findings. Permission bits are
  plain `u64` values (`PERM_*` constants) so no Discord types leak in.
- `evaluate_permissions`: the creator category and one channel inside it each
  resolve from the guild base with their own overwrite rows, as Discord does
  (a synced channel carries copies of the category rows; nothing stacks).
  Each Manage Channels / Move Members / Manage Roles / View Channel missing
  at either level yields one finding at the outermost responsible level:
  guild base, the category overwrite (naming `category_id`), or the channel
  overwrite. Overwrite allows rescue a missing guild base only where they
  apply; unrelated role/member rows are ignored.
- `PermissionFinding`: enums plus `category_id`/`channel_id` only — safe to
  render into a notice or `/setup` listing. No name, message text, URL or
  token can be represented here.
- `notice_target`: V10 fallback order from a caller-supplied availability
  snapshot — guild system channel (with the last setup user for the mention),
  then DM to that user, then DM to the guild owner, then the creator
  channel's chat. Returns `None` only when nothing is available.
- `NoticeThrottle`: pure repeat state over caller-supplied timestamps —
  **N = 3 sends** (`NOTICE_MAX_SENDS`), first notice immediately, repeats
  after 5 then 30 minutes (`NOTICE_BACKOFF_MS`). Exhausted failures stay
  listed but silent. The caller persists state, supplies `now_ms`, calls
  `observe` for every failure each health check detects (tracked with no
  sends, so it is listed and due even when `notice_target` returns `None` or
  the send fails), records actual sends, and resolves failures the check no
  longer reports, so a recurrence starts a fresh budget. `current_failures`
  backs the `/setup` failure list in deterministic order.

The caller supplies authoritative permission snapshots, overwrite rows, bot
identity and candidate availability, authenticates actor identity, chooses
display wording, sends notices, and persists throttle state. Runtime wiring
stays on the parent V10 card, behind its V1 edge, and consumes this module.
