# Voice cutover rehearsal (staging)

Rehearsal procedure for retiring the interim voice bot and moving its rooms
to two-bot-next. Staging guild only. No live guild changes. This document
describes the rehearsed steps; the production authorization and sequencing
live on the cutover card, not here.

Source of truth for behavior: `docs/voice-rooms.md` §V1 (lifecycle), §V8
(placement/permissions/defaults), §V11 (export/import) and the Discord API
notes (rename limits, idempotent events, ordered per-guild queue, 429
retry-after). Command wiring: `crates/bot/src/voice_rooms.rs`
(`parse_voice_command`, `plan_import_preview`, `plan_import_confirm`,
`plan_room` in `voice_room_plan.rs`, `reconcile`). Config contract:
`docs/voice-config-codec.md`; persistence: `crates/cutover/src/voice_config_store.rs`.

## 1. Channel-to-creator mapping

For each interim-bot generator/lobby channel that should survive cutover,
record one row. Fill this table on staging first, then reuse it for the live
guild at cutover time.

| Interim source | Rust creator channel | Category | Template | Default limit | Privacy default | Text channel | Position |
|---|---|---|---|---|---|---|---|
| (interim generator name) | (staging voice channel) | (category) | (name template or default) | (number, 0 = unlimited) | (public/private) | (on/off) | (above/below, first number) |

Rules the rehearsal verified in source:

- `/create <name>` marks a voice channel as a creator with spec defaults
  (creator-source permissions, inherit limit, public, no text channel, rooms
  above, numbering from 1). If the database write fails after the channel is
  created, the handler deletes the channel again (compensation); if that
  delete also fails it names the channel so an admin can remove it by hand.
- Per-creator tuning after creation runs through the V11 import (the whole
  configuration document), because the individual settings commands
  (`/position`, `/group`, `/inheritpermissions`, `/defaultlimit`,
  `/alwaysprivate`) are not wired as slash commands. There is no partial
  per-creator edit path: change the value in the exported JSON and re-import.
- `group_by_category` (shared numbering and contiguous block per category)
  is stored by the V11 import and shown in the diff, but the room planner
  hardcodes ungrouped placement. Do not promise shared numbering at cutover
  until that wiring lands; map each interim generator to its own creator
  channel instead.

## 2. Template/settings map (V11 export/import)

1. On the staging guild, run `/export` (Manage Server). The bot replies
   ephemerally with `voice-config-guild-<guild>-v1.json`. The document holds
   creators, permanent-channel templates, aliases, named lists, logging and
   guild settings; it never holds rooms, ownership, nicknames or bitrate
   preferences.
2. Edit the JSON for the cutover mapping (channel IDs must be the staging
   guild's own; cross-guild IDs are refused, unknown IDs are reported and
   skipped from the candidate, never silently applied).
3. Run `/import` with the file attached. The bot shows a diff preview
   (capped at 20 lines) with Confirm/Cancel. Confirm re-reads current state
   and re-diffs: if anything changed underneath, the stale preview is
   replaced, never applied blind. Cancel and expiry write nothing.
4. Limits: 256 KiB per file; malformed JSON is refused with line/column
   only; empty diffs answer with a notice and store nothing.
5. Round-trip safety: `snapshot -> export -> import -> apply -> snapshot` is
   the identity. An unset default limit ("inherit the creator channel")
   exports as 0 and re-applying 0 over an unset value keeps it unset, so a
   plain export/import never turns "inherit" into "unlimited".

Rehearsed offline against the merged handlers (no Discord writes): preview
refuses oversized/malformed uploads before any write, unknown channels are
pruned from the candidate while the diff still reports them, and Confirm
revalidates against freshly read state. Live-guild acceptance (actual
`/export` download and `/import` apply on staging) is still open and needs
a staging identity with Manage Server on the staging guild.

## 3. Quiet-hours window proposal

Cutover should land in the guild's quietest voice window so create/move/
delete races and member confusion stay minimal. Proposal, to confirm with
moderators before the live cutover:

- Window: a weekday early-morning UTC slot (exact hour set from observed
  staging/live voice activity; keep it under 60 minutes).
- Announce a maintenance window to moderators the day before (state, window,
  affected features, manual moderation contact), following the communication
  template in `docs/cutover.md`.
- Freeze new room creation just before the swap (guild creation toggle via
  `/access`), reconcile, then enable creators after the interim bot leaves.
- Record freeze time, first-ready time and the watch handoff.

This proposal is not approval: the live window needs moderator sign-off on
the cutover card.

## 4. Practice create/move/delete plus ghost-channel check (staging)

Run with a staging identity once staging is healthy. Each step records a
UTC timestamp; timings below are the targets to beat.

1. Join the staging creator channel: exactly one room appears and the member
   lands in it (target: seconds). Two simultaneous joins yield two rooms.
2. Leave: the room is deleted within seconds of emptying. A hand-deleted
   room is quietly forgotten, never re-deleted.
3. Move/rename probes: owner `/limit`, `/name`, `/private`, `/public`
   round-trip with ephemeral replies; companion Join channel follows the
   owner and dies with the room.
4. Ghost-channel check: compare tracked rooms against the channels that
   actually exist. Tracked-but-gone rooms are forgotten; channels the bot
   never tracked are never touched. The runtime `reconcile` pass does this
   at startup and after reconnects; the read-only
   `report voice-reconcile --guild <id>` CLI covers funnel event halves
   (session start/end), not room tracking, so the ghost check itself is a
   tracked-vs-live comparison, currently via `/setup` health plus worker
   logs. A dedicated count query is an open gap (see §6).
5. Rename-rate-limit behavior: at most ~2 renames per 10 minutes per
   channel; the bot coalesces to one pending name and never delays
   create/delete behind a rename backlog.

## 5. Rehearsal results (2026-10-03)

Environment at rehearsal time:

- Staging `/health` 200 (`ok`); `/readyz` 503 with `gateway=starting`,
  `database=ready`, build at the current main head. The staging deploy
  pipeline was red at the rollout gate (`rollout_timeout`: container active
  but never healthy inside the window) across several consecutive main
  pushes, and main `check` was red on the supply-chain SBOM gate at the
  same head. No live staging practice was possible in this state, and no
  staging writes were attempted.
- Local compile pool refused (no idle slot); per repo policy the run did
  not evade it. This change is docs-only, so hosted CI is the verifier.

What was rehearsed and passed (offline, from merged source):

- V11 preview/confirm state machine refuses oversized and malformed
  uploads before any write, prunes unknown channels from the candidate
  while still reporting them, and revalidates on Confirm.
- Export/apply round-trip preserves the unset default limit.
- `/create` compensates a failed database write by deleting the new
  channel, and names it for manual cleanup when the delete fails.
- Reconcile forgets hand-deleted rooms and never touches untracked
  channels.

## 6. Gaps

Each gap below is filed as its own card and linked from the rehearsal
issue. Live staging practice (§4 live run) is a follow-up blocked on
staging returning to healthy; it is not listed here as a code gap.

1. Shared/category numbering (`/group`) is stored but not honored: the
   room planner hardcodes ungrouped placement, so an import carrying
   `group_by_category` changes nothing at runtime.
2. No dedicated ghost-channel count for cutover verification: room
   tracking diffs live only in the runtime reconcile pass, surfaced via
   `/setup` health and logs; there is no read-only count an operator can
   poll across the swap.
3. The per-creator settings commands (`/position`,
   `/inheritpermissions`, `/defaultlimit`, `/alwaysprivate`) are not wired,
   so mid-cutover tuning requires a full-document export/edit/import
   cycle. Usable, but slower and easier to mistype under time pressure
   than single-setting commands.
