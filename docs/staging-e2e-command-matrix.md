# Staging E2E command-scope matrix

Contract for the future staging-guild smoke suite (parent epic: staging-guild
E2E smoke for core slash commands). It defines WHAT the live suite covers;
the harness slices define HOW it runs. Derived from code, not from a guild:
`crates/core/src/e2e_matrix.rs` (`e2e_command_matrix`) built on
`InteractionRouter::publish_set` (all gates on),
`command_permissions::command_permission`, and `route_slash` denial order.
The `e2e_matrix_coverage` integration test fails when a command ships without
a matrix row and when this doc drops one.

Scope: 28 built-in slash commands (core + scorecard + automation +
announcement + moderation — the set `docs/commands.md` renders). No guild was
touched, no staging secrets used, no credentials created.

Staging env the live suite needs: `TWO_COMMUNITY_SCORECARD=1`,
`TWO_AUTOMATIONS=1`, `TWO_ANNOUNCEMENTS=1`, `TWO_MODERATION=1` on the
staging host so every row below is published. The suite asserts the ACK
contract from `docs/interaction-replies.md` for every probe: first callback
within the 3 s budget (type 4 or type 5 + PATCH `@original`), mentions
suppressed, content ≤2000 scalars; failures surface only the generic
`Something went wrong (ref …)` text, never internals.

Shared denial paths (every row): foreign or missing guild → moderation
answers `GuildRestricted` (`This command is restricted to the configured
guild.`), all other builtins stay silent (`Ignore`); stale or unknown names
→ ephemeral `UNKNOWN_COMMAND_REPLY`; handler failure → generic ref error
(watch-log class only for the store path: `store_unavailable`).

## Matrix

| # | Command | Gate (staging env) | Permission | Success shape | Denial paths |
| --- | --- | --- | --- | --- | --- |
| 1 | `/rank` | always | Everyone | Ephemeral text with XP, level, server rank | — |
| 2 | `/leaderboard` | always | Everyone | Public mention-suppressed top ten | — |
| 31 | `/help` | always | Everyone | Immediate ephemeral list of the live published commands, grouped by audience with permission hints (no defer, no store read) | — |
| 12 | `/attendance` | `TWO_COMMUNITY_SCORECARD` | ManageEvents | Ephemeral confirmation/refusal; records verified attendee | `ScorecardDisabled`, `ManageEventsRequired`, `event-occurrence` required ≤128, `member` required |
| 14 | `/command` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral confirmation + audit row | `AutomationsDisabled`, `ManageServerRequired`, `name`/`template` required |
| 15 | `/command-remove` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral confirmation + audit row | `AutomationsDisabled`, `ManageServerRequired`, `name` required |
| 16 | `/command-list` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral custom-command list | `AutomationsDisabled`, `ManageServerRequired` |
| 17 | `/schedule` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral defer then completion; one timing option required | `AutomationsDisabled`, `ManageServerRequired`, `body` required, `in-minutes` 1–525600, `every-minutes` 60–525600 |
| 18 | `/schedule-remove` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral confirmation + audit row | `AutomationsDisabled`, `ManageServerRequired`, `id` required |
| 19 | `/schedule-list` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral schedule list | `AutomationsDisabled`, `ManageServerRequired` |
| 20 | `/sticky` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral defer then confirmation + audit row | `AutomationsDisabled`, `ManageServerRequired`, `body` required, `debounce` 1–300 |
| 21 | `/sticky-remove` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral confirmation + audit row | `AutomationsDisabled`, `ManageServerRequired` |
| 24 | `/rsvp` | `TWO_ANNOUNCEMENTS` | Everyone | Ephemeral `RSVP saved: <status>.` | `AnnouncementsDisabled`, `event-id` required, `status` going/interested/declined |
| 25 | `/rsvp-attendance` | `TWO_ANNOUNCEMENTS` | Everyone | Ephemeral RSVP totals | `AnnouncementsDisabled`, `event-id` required |
| 26 | `/lfg` | `TWO_ANNOUNCEMENTS` | ManageEvents | Ephemeral signup post + audit row | `AnnouncementsDisabled`, `ManageEventsRequired`, `title`/`starts-at`/`roles` required |
| 27 | `/lfg-close` | `TWO_ANNOUNCEMENTS` | ManageEvents | Ephemeral confirmation + audit row | `AnnouncementsDisabled`, `ManageEventsRequired`, `id` required |
| 28 | `/feed-add` | `TWO_ANNOUNCEMENTS` | ManageGuild | Ephemeral defer then confirmation + audit row | `AnnouncementsDisabled`, `ManageServerRequired`, `kind` rss/youtube/twitch, `source` required |
| 29 | `/feed-remove` | `TWO_ANNOUNCEMENTS` | ManageGuild | Ephemeral confirmation + audit row | `AnnouncementsDisabled`, `ManageServerRequired`, `id` required |
| 30 | `/feed-list` | `TWO_ANNOUNCEMENTS` | ManageGuild | Ephemeral relay list | `AnnouncementsDisabled`, `ManageServerRequired` |
| 3 | `/ban` | `TWO_MODERATION` | BanMembers | Ephemeral outcome + REST ban + audit reason | `GuildRestricted`, `ModerationDisabled`, `ModerationPermission(Ban)`, member-moderation policy, `target`/`reason` required |
| 4 | `/tempban` | `TWO_MODERATION` | BanMembers | Ephemeral outcome + timed ban + audit reason | `GuildRestricted`, `ModerationDisabled`, `ModerationPermission(TempBan)`, member-moderation policy, `duration_seconds` ≥60 (runtime max 365d) |
| 5 | `/kick` | `TWO_MODERATION` | KickMembers | Ephemeral outcome + REST kick + audit reason | `GuildRestricted`, `ModerationDisabled`, `ModerationPermission(Kick)`, member-moderation policy, `target`/`reason` required |
| 6 | `/timeout` | `TWO_MODERATION` | ModerateMembers | Ephemeral outcome + REST timeout + audit reason | `GuildRestricted`, `ModerationDisabled`, `ModerationPermission(Timeout)`, member-moderation policy, `duration_seconds` ≥60 (runtime max 28d) |
| 7 | `/warn` | `TWO_MODERATION` | ModerateMembers | Ephemeral outcome + recorded warning | `GuildRestricted`, `ModerationDisabled`, `ModerationPermission(Warn)`, member-moderation policy, `target`/`reason` required |
| 8 | `/purge` | `TWO_MODERATION` | ManageMessages | Ephemeral outcome + message delete | `GuildRestricted`, `ModerationDisabled`, `ModerationPermission(Purge)`, `count` 1–100, `reason` required ≤512 |
| 9 | `/slowmode` | `TWO_MODERATION` | ManageChannels | Ephemeral outcome + slowmode effect (0 disables) | `GuildRestricted`, `ModerationDisabled`, `ModerationPermission(Slowmode)`, `seconds` 0–21600, `reason` required ≤512 |
| 10 | `/lockdown` | `TWO_MODERATION` | ManageChannels | Ephemeral outcome + send block | `GuildRestricted`, `ModerationDisabled`, `ModerationPermission(Lockdown)`, `reason` required ≤512 |
| 11 | `/unlock` | `TWO_MODERATION` | ManageChannels | Ephemeral outcome + send restore | `GuildRestricted`, `ModerationDisabled`, `ModerationPermission(Unlock)`, `reason` required ≤512 |

Member-moderation policy order (ban, tempban, kick, timeout, warn):
permission → target present → self-target → owner/Owen/bot/staff protection →
bot hierarchy → actor hierarchy. Audit `reason` is trimmed, non-empty, ≤512
UTF-16 units on every moderation row.

## Out of scope

- Voice `/create /setup /ping /invite /textchannels /access /reclaim
  /transfer /logging /export /import /position /group /inheritpermissions
  /defaultlimit /alwaysprivate /kick /name /private /public /limit /unlimit`
  (separate `TWO_VOICE` slice; its
  `kick` loses the merge to moderation first-wins — runtime dispatch decides).
  The published voice set is pinned separately by the `voice` section of
  `crates/core/tests/fixtures/staging_published_commands.json`.
- `/templateassistant` (voice + assistant gates; same fixture section).
- DB-backed custom commands and `!` prefix triggers (dynamic surfaces).
- Component/modal surfaces (pickers, tickets, self-roles, LFG signup).
- Dropped `/rota-acknowledge` (parity row 13, never published).

## Live-suite follow-up

The live run belongs to the parent staging-guild smoke epic: run each row's
success probe plus one denial probe per gate/permission group against the
staging guild, and file results there. Safe denial probes (unknown name, gate
off, permission denied, invalid option) must assert the exact refusal text and
no watch-log row; only the store-unavailable path may file `store_unavailable`.
