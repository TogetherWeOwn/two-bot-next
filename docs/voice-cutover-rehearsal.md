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
- Per-creator tuning after creation runs through the single-setting
  commands (`/position`, `/group`, `/inheritpermissions`, `/defaultlimit`,
  `/alwaysprivate`), each an admin-gated write of one field with the same
  bounds the import validates. The V11 import remains for bulk edits
  (templates, aliases, lists, logging): change the value in the exported
  JSON and re-import.
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
4. Ghost-channel check: poll the read-only
   `report voice-ghosts --guild <staging-guild-id>` count across the swap.
   It diffs tracked `voice_rooms` rows against the guild's live voice
   channels (one SELECT plus one channel-listing GET, no writes) and prints
   `tracked_present`, `tracked_gone` (the runtime `reconcile` forget class)
   and `untracked_present` plus a `clean` flag: poll until `tracked_gone`
   and `untracked_present` are both empty. Tracked-but-gone rooms are
   forgotten; channels the bot never tracked are never touched — the report
   lists them for manual triage. Existence only: occupant and manageability
   classes need a gateway-derived snapshot (pair the id lists with live
   occupancy when building a cleanup snapshot), and the sibling
   `report voice-reconcile` CLI covers funnel event halves (session
   start/end), not room tracking. `--seed` prints a fixture demo with no
   database and no Discord.
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

### Ghost-count staging verification (2026-10-03, read-only)

Head `87d98060`. Staging guild only: one `voice_rooms` SELECT attempt,
read-only legacy temp-voice counts, one channel-listing GET. No writes,
no cleanup actions, no live guild queries.

- The `voice_rooms` table is absent on the staging database (zero
  `voice_*` tables; migrations pending), so the tracked-rows SELECT the
  live `report voice-ghosts` count needs fails with `UndefinedTable`.
  The diff is unverifiable until staging migrates.
- Legacy temp-voice rows for staging: `temp_voice_channels` 0 (no legacy
  ghosts), `temp_voice_creates` 1 (dated 2026-09-29), `temp_voice_audit`
  24 with the latest a 2026-09-29 boot-reconcile delete. No post-cleanup
  run is recorded.
- The live listing shows 3 voice channels (`Lobby`, `Squad`, `Voice 1`).
  With zero tracked rows the arithmetic gives `tracked_gone=[]`,
  `untracked_present=[3]`, `clean=false`.
- Candidate residual baseline: the 3 live voice channels read as
  permanent staging voice, but no baseline documents them as residual
  yet, so the count cannot certify "back to baseline".
- Verdict: NEEDS WORK — the count is not at baseline (expected
  `clean=true` or a documented residual). Needs staging migrated and
  healthy plus a written residual baseline naming the permanent
  channels. The baseline is now written below (2026-10-04); the staging
  migration and health preconditions still stand.

### Documented residual baseline: permanent staging voice channels

Recorded 2026-10-04 from the staging gate-on plan. This is the written
baseline the 2026-10-03 verdict asked for. Channel ids live on the private
tracking card, not in this public repo.

The staging guild (`TWO Staging`) keeps three permanent voice channels
that the bot has never tracked:

| Channel | Role |
|---|---|
| `Lobby` | Permanent staging voice, zero `voice_rooms` rows |
| `Squad` | Permanent staging voice, zero `voice_rooms` rows |
| `Voice 1` | Permanent staging voice, zero `voice_rooms` rows |

- They are safe when `TWO_VOICE=1` ships. `reconcile` walks only the rooms
  the store tracks (`self.rooms.keys()` in `VoiceRooms::reconcile`,
  `crates/bot/src/voice_rooms.rs`), so a channel with no tracked row is
  never visited, renamed or deleted. No code change is needed.
- `report voice-ghosts` treats every live voice channel without a
  `voice_rooms` row as `untracked_present`, creator channels included
  (`count_ghosts` and `is_live_voice_kind` in
  `crates/cutover/src/voice_ghosts.rs`). Marking a channel as a creator
  with `/create` does not make it tracked; only rooms the bot spawns are.
  So `clean=true` is unreachable on this guild by design.
- Expected reading before any creator exists: `tracked_present=[]`,
  `tracked_gone=[]`, `untracked_present=[3]`, `clean=false`. That is the
  accepted residual, not a ghost.
- Acceptance for a live rehearsal swap: `tracked_gone=[]`, and
  `untracked_present` is exactly the three permanent channels plus the
  staging creator channel(s) the rehearsal designates (count 3 + creators).
  Spawned rooms appear in `tracked_present` while occupied and must leave
  it once emptied and deleted. Any other untracked channel, or any
  tracked-but-gone row, is a real ghost and fails the check. Record the
  designated creator in the run record; if the creator is one of the three
  permanent channels, the count stays 3.
- Order of enablement: migrations 0413 and 0416 apply on staging first,
  then the `TWO_VOICE=1` binding ships, then the voice commands publish.
  The live smoke additionally needs a second human account.

### Live staging rehearsal (2026-10-04, read-only)

Revision `5a42c357` (main at run time); the staging Worker served build
`37240415572-1`. Staging guild only: the guild fence passed (the configured
guild id equals the pinned staging id, not the live one). The only
credential was the staging bot token, used for GET requests. No writes, no
interactions, no database access, no live guild queries.

| Step | Result |
|---|---|
| Staging readiness | `scripts/qa_cutover_probes.py --expect-ready`: 5/5. `process`, `gateway`, `database` and `token_invalid` all `ready`; 12 jobs reported. The deploy pipeline that was red on 2026-10-03 is green and the gateway is up. |
| Dual-run detection | The guild member list holds one bot: the staging application, matching the pinned application id. No second bot account is in the guild, so nothing else can be managing voice channels. This is the negative baseline only: staging never had an interim voice bot, so the positive case (two voice-managing bots) cannot be rehearsed here. At cutover the same read must show the interim bot until it is removed, then only ours. |
| Command failover | Not exercisable. The staging guild registry holds 17 commands (rank, leaderboard, lfg, rsvp, schedule, feed, sticky, custom `command` family) and the global registry is empty. None of the voice commands (`/create`, `/setup`, `/export`, `/import`, `/limit` and the rest) is registered. They publish only behind `TWO_VOICE=1`, which the committed staging config already sets (`[env.staging.vars]` in `wrangler/wrangler.toml`); the registry wiring is still open in PR #559. With nothing published there is no command surface to fail over to, and no name collision with the interim bot's commands can be observed. |
| Ghost baseline | The guild lists the same three permanent voice channels as 2026-10-03 (`Lobby`, `Squad`, `Voice 1`, created 2026-09-06 and 2026-09-07); nothing new appeared. The `report voice-ghosts` count was not run: the staging database binding available to the operator tooling is the legacy staging database, where `voice_rooms` does not exist. The arithmetic matches the documented residual baseline above: `untracked_present=[3]`, `clean=false` accepted, and `clean=true` unreachable on this guild by design. |
| Handoff checklist (template export/map, quiet hours, stop/remove interim, enable creators, create/move/delete verify, token revoke, secret delete) | Not executed. `/export`, `/import` and `/create` are unpublished; the staging guild has one human member and no identity that can join a voice channel on demand; stop/remove interim and the credential steps are production steps that the cutover card gates. Quiet-hours selection needs live voice activity, which staging (two members) cannot show. |

Tooling finding: `scripts/qa_cutover_probes.py` sent urllib's default agent,
which the staging edge refuses (Cloudflare error 1010, every route 403), so
the probe could not grade the staging Worker at all. It now names itself
(`two-bot-next-staging-rollout/1.0`, shared with `scripts/staging_rollout.py`);
a loopback regression test pins the agent.

Verdict: **NO-GO** for retiring the interim bot. Staging is healthy again,
but the live steps this rehearsal exists to practice are blocked by four
things, in order:

1. Voice commands must publish on staging: merge PR #559 (`TWO_VOICE=1` is already set in the committed staging config).
2. The ghost count needs the staging database binding that carries the
   migrated `voice_*` tables.
3. A staging identity with Manage Server (for `/export`/`/import`) that can
   also join and leave a voice channel on demand.
4. A written residual baseline naming `Lobby`, `Squad` and `Voice 1` — recorded above (expected `untracked_present=[3]`, `clean=false`; `clean=true` unreachable by design); apply its pass rule to the live run.

When all four hold, run §4 live and record timestamps in this section.

Re-check at `86a6668a7`: the 2026-10-04 registry observation above is history,
not current state. The code wiring it waited on has since landed — the voice
set is defined in `voice_commands()` (`crates/core/src/voice_rooms.rs:1292-1660`),
merged when the voice gate is on (`crates/core/src/router.rs:717-719`), and
synced on ready (`crates/bot/src/command_runtime.rs:1009-1020`). Re-read the
staging guild registry before the next rehearsal instead of reusing the
17-command snapshot.

## 6. Gaps

Each gap below is filed as its own card and linked from the rehearsal
issue. Live staging practice (§4 live run) is a follow-up blocked on the
four items in the 2026-10-04 verdict above; it is not listed here as a code
gap.

1. ~~Shared/category numbering (`/group`) is stored but not honored: the
   room planner hardcodes ungrouped placement, so an import carrying
   `group_by_category` changes nothing at runtime~~ Resolved in code:
   `group_by_category` is honored (`crates/bot/src/voice_rooms.rs:3427-3444`,
   `crates/bot/src/voice_room_plan.rs:206-212`). Still open: staging practice
   of shared numbering on the staging guild (§4 live run). Re-checked at
   `86a6668a7`.
2. ~~No dedicated ghost-channel count for cutover verification~~ Shipped:
   `report voice-ghosts --guild <id>` is the pollable read-only count
   (tracked-present, tracked-gone, untracked-present plus a `clean` flag),
   covered by unit and CLI integration tests. Still open: live staging
   polling of the count across a rehearsal swap (§4 live run).
3. ~~The per-creator settings commands are not wired, so mid-cutover
   tuning requires a full-document export/edit/import cycle~~ Wired: the
   per-creator settings commands (`/position`, `/group`,
   `/inheritpermissions`, `/defaultlimit`, `/alwaysprivate`) are
   admin-gated slash commands with single-field writes, covered by unit
   tests. Still open: live staging practice of each command on the
   staging guild once staging is healthy (§4 live run).

## 7. Staging gate (`TWO_VOICE`)

The staging Worker binds `TWO_VOICE = "1"` under `[env.staging.vars]` in
`wrangler/wrangler.toml`. The var is forwarded into the container
(`FORWARDED_FLAGS`, `wrangler/src/container-env.ts`) and `build_voice_runtime`
attaches the voice sink to the gateway only when it is exactly `1`.
`scripts/check-env-bindings.py` fails a top-level or production declaration,
so production voice stays an Operator-approved binding.

- **Order matters.** The voice store reads the migrated voice tables, so the
  staging ledger must already carry the voice migrations (0224-0229,
  0412-0414 and 0416-0418) before the var ships: apply through `staging-migrate`,
  then re-apply the role plan so the runtime role holds the new tables. Merge
  the flip only after a post-apply plan shows no pending migrations.
- **Check the custom-command count first.** The 100-command guild limit leaves
  room for 50 stored custom commands beside the published builtins, voice set
  and `/templateassistant`. A guild that already stores more makes the gateway
  registry sync refuse (`TotalLimit`) and the gateway task fail at boot, rather
  than degrading; trim the extra rows before the flip.
- **Permanent channels stay untouched.** `reconcile` iterates the rooms the
  store tracks and nothing else, so the guild's three permanent voice
  channels (`Lobby`, `Squad`, `Voice 1`) are never deleted: with zero tracked
  rows the live ghost count reads `untracked_present=[3]`, which is the
  documented residual baseline for staging until a creator channel exists.
- **Commands publish with the sink.** The gate attaches the sink and the
  reconciler, and the guild registry gains the voice commands through
  `InteractionRouter::publish_set` (`RouterGates::voice`) while the sink is
  built. The deployed registry read-back is staging acceptance. Live
  create/move/delete practice (§4) also needs a second human account in the
  staging guild.
- **Rollback.** Delete the `TWO_VOICE` line and redeploy `deploy-staging`.
  Tracked rooms stay in the database; any whose channel has gone are
  forgotten by the first reconcile after the gate is back on.
