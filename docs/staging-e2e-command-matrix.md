# Staging E2E command-scope matrix

Contract for the future staging-guild smoke suite (parent epic: staging-guild
E2E smoke for core slash commands). It defines WHAT the live suite covers;
the harness slices define HOW it runs. Derived from code, not from a guild:
`crates/core/src/e2e_matrix.rs` (`e2e_command_matrix`) built on
`InteractionRouter::publish_set` (all gates on),
`command_permissions::command_permission`, and `route_slash` denial order.
The `e2e_matrix_coverage` integration test fails when a command ships without
a matrix row and when this doc drops one.

Scope: 47 built-in slash commands (core + scorecard + automation +
announcement + moderation — the commands `docs/commands.md` renders ahead of
the voice set; with the voice gates on it renders 68). No guild was
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
| 32 | `/privacy` | always | Everyone | Deferred ephemeral reply; analytics `opt-out` purges the invoker's guild rows plus suppression plus one audit receipt, `opt-in` clears suppression, `status` reports suppression plus guild switches; `delete` queues the invoker's erasure request (fresh requests acknowledge as pending; same-interaction retries reuse the request and direct the member to status), and deletion status/receipt reads bind to the same invoker and guild | subcommand required (`opt-out`/`opt-in`/`status`/`delete`/`deletion-status`/`deletion-receipt`); request id required for `deletion-status` and `deletion-receipt`; guild-only |
| 34 | `/reopen` | always | Everyone | Deferred ephemeral reply; a fresh private games or session menu built from current roles and current configured picks, with zero role, channel or funnel writes; games includes a no-change preview select | `kind` required (`games`/`session`); unavailable kinds and failed lookups get private no-effect guidance, guild-only |
| 42 | `/welcome-preview` | always | ManageGuild | Deferred private configuration preview through the live renderer for roleless synthetic newcomer `765432109876543210`; no delivery or member lookup | `ManageServerRequired`; disabled or invalid configuration gets private no-effect/retry guidance |
| 43 | `/game-mix` | always | ManageGuild or Administrator | Ephemeral defer then one guild aggregate page: up to ten games per `page`, `play`/`voice` ordering, JSON by default or a readable `summary`; play/in-voice milliseconds and session counts; empty hint or explicit label truncation note; at most 2000 UTF-16 units | `ManageServerRequired`; live guild explicitly refused before read; store failure gives private retry guidance, no fabricated totals; foreign guild/DM ignored |
| 12 | `/attendance` | `TWO_COMMUNITY_SCORECARD` | ManageEvents | Ephemeral confirmation/refusal; records verified attendee | `ScorecardDisabled`, `ManageEventsRequired`, `event-occurrence` required ≤128, `member` required |
| 14 | `/command` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral confirmation + audit row | `AutomationsDisabled`, `ManageServerRequired`, `name`/`template` required |
| 15 | `/command-remove` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral confirmation + audit row | `AutomationsDisabled`, `ManageServerRequired`, `name` required |
| 16 | `/command-list` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral custom-command list | `AutomationsDisabled`, `ManageServerRequired` |
| 33 | `/command-inspect` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral defer then one private content-only view (summary, saved template, or rendered preview); no send, registration, cooldown, mutation or run audit | `AutomationsDisabled`, `ManageServerRequired`, `name` required, `view` summary/template/preview, `arguments` ≤500 |
| 40 | `/command-disable` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral defer then confirmation after the saved definition is paused + audit row; a repeated pause is an explicit no-op without republish | `AutomationsDisabled`, `ManageServerRequired`, `name` required |
| 41 | `/command-enable` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral defer then confirmation after the saved definition is resumed + audit row; a repeated resume is an explicit no-op without republish | `AutomationsDisabled`, `ManageServerRequired`, `name` required |
| 17 | `/schedule` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral defer then completion; one timing option required | `AutomationsDisabled`, `ManageServerRequired`, `body` required, `in-minutes` 1–525600, `every-minutes` 60–525600 |
| 18 | `/schedule-remove` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral confirmation + audit row | `AutomationsDisabled`, `ManageServerRequired`, `id` required |
| 19 | `/schedule-list` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral schedule list | `AutomationsDisabled`, `ManageServerRequired` |
| 48 | `/schedule-inspect` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral defer then one private content-only view (saved metadata plus edit revision, or the whole saved body); no mutation or audit | `AutomationsDisabled`, `ManageServerRequired`, `id` required, `view` metadata/body |
| 49 | `/schedule-edit` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral defer then save confirmation + one content-free audit row; only body/updater/timestamp change, only on a matching idle revision | `AutomationsDisabled`, `ManageServerRequired`, `id` required, `revision` required, `body` required |
| 20 | `/sticky` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral defer then confirmation + audit row | `AutomationsDisabled`, `ManageServerRequired`, `body` required, `debounce` 1–300 |
| 21 | `/sticky-remove` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral confirmation + audit row | `AutomationsDisabled`, `ManageServerRequired` |
| 37 | `/giveaway` | `TWO_AUTOMATIONS` | ManageGuild | Ephemeral defer then confirmation + audit row; one-click member entry; timed draw + announce with reroll | `AutomationsDisabled`, `ManageServerRequired`, `prize` 1–200 chars, `duration` 1–43200 min, `winners` 1–100 |
| 24 | `/rsvp` | `TWO_ANNOUNCEMENTS` | Everyone | Ephemeral `RSVP saved: <status>.`; a changed or repeated choice adds `(changed from <previous>)` or `(saved again; time updated)` (pasted links append the event id) | `AnnouncementsDisabled`, `event-id` required (id or pasted link), `status` going/interested/declined |
| 25 | `/rsvp-attendance` | `TWO_ANNOUNCEMENTS` | Everyone | Ephemeral RSVP totals | `AnnouncementsDisabled`, `event-id` required (id or pasted link) |
| 26 | `/lfg` | `TWO_ANNOUNCEMENTS` | ManageEvents | Ephemeral signup post + audit row | `AnnouncementsDisabled`, `ManageEventsRequired`, `title`/`starts-at`/`roles` required |
| 27 | `/lfg-close` | `TWO_ANNOUNCEMENTS` | ManageEvents | Ephemeral confirmation + audit row | `AnnouncementsDisabled`, `ManageEventsRequired`, `id` required |
| 26 | `/lfg-edit` | `TWO_ANNOUNCEMENTS` | ManageEvents | Ephemeral saved-edit receipt with canonical start time; refreshes the existing post, preserving signups | `AnnouncementsDisabled`, `ManageEventsRequired`, `id` required ≤128; `title` ≤100 or future ISO-8601 `starts-at` required; same-guild open post only |
| 38 | `/lfg-list` | `TWO_ANNOUNCEMENTS` | ManageEvents | Ephemeral current-channel open confirmed posts, at most five whole rows, bounded cursor continuation; no mutations | `AnnouncementsDisabled`, `ManageEventsRequired`, optional `cursor` ≤512 bound to guild/channel/query, invalid or stale page → restart |
| 28 | `/feed-add` | `TWO_ANNOUNCEMENTS` | ManageGuild | Ephemeral defer then confirmation + audit row | `AnnouncementsDisabled`, `ManageServerRequired`, `kind` rss/youtube/twitch, `source` required |
| 29 | `/feed-remove` | `TWO_ANNOUNCEMENTS` | ManageGuild | Ephemeral confirmation + audit row | `AnnouncementsDisabled`, `ManageServerRequired`, `id` required |
| 44 | `/feed-pause` | `TWO_ANNOUNCEMENTS` | ManageGuild | Ephemeral confirmation + audit row; enabled-to-paused transition refused while pending recovery exists | `AnnouncementsDisabled`, `ManageServerRequired`, exactly one string `id` required |
| 45 | `/feed-resume` | `TWO_ANNOUNCEMENTS` | ManageGuild | Ephemeral confirmation + audit row | `AnnouncementsDisabled`, `ManageServerRequired`, exactly one string `id` required |
| 30 | `/feed-list` | `TWO_ANNOUNCEMENTS` | ManageGuild | Ephemeral relay list | `AnnouncementsDisabled`, `ManageServerRequired` |
| 39 | `/feed-inspect` | `TWO_ANNOUNCEMENTS` | ManageGuild | Ephemeral relay detail with safe source and last attempt | `AnnouncementsDisabled`, `ManageServerRequired`, `id` required |
| 3 | `/ban` | `TWO_MODERATION` | BanMembers | Ephemeral outcome + REST ban + audit reason | `GuildRestricted`, `ModerationDisabled`, `ModerationPermission(Ban)`, member-moderation policy, `target`/`reason` required |
| 4 | `/tempban` | `TWO_MODERATION` | BanMembers | Ephemeral outcome + timed ban + audit reason | `GuildRestricted`, `ModerationDisabled`, `ModerationPermission(TempBan)`, member-moderation policy, `duration_seconds` ≥60 (runtime max 365d) |
| 46 | `/tempban-list` | `TWO_MODERATION` | BanMembers (4) | Ephemeral saved-expiry inventory: ten complete rows per page, localized saved due time, exact state and dispatch uncertainty. Saved/due is not a confirmed Discord unban; running is not completion. Fresh actor authority, suppressed mentions, at most 2000 UTF-16 units; no claims, audit writes or moderation effects. | `GuildRestricted`, `ModerationDisabled`, `TempbanListPermission`, optional integer `page` 1–10000 (default 1); no target or reason |
| 5 | `/kick` | `TWO_MODERATION` | KickMembers | Ephemeral outcome + REST kick + audit reason | `GuildRestricted`, `ModerationDisabled`, `ModerationPermission(Kick)`, member-moderation policy, `target`/`reason` required |
| 6 | `/timeout` | `TWO_MODERATION` | ModerateMembers | Ephemeral outcome + REST timeout + audit reason | `GuildRestricted`, `ModerationDisabled`, `ModerationPermission(Timeout)`, member-moderation policy, `duration_seconds` ≥60 (runtime max 28d) |
| 7 | `/warn` | `TWO_MODERATION` | ModerateMembers | Ephemeral outcome + recorded warning | `GuildRestricted`, `ModerationDisabled`, `ModerationPermission(Warn)`, member-moderation policy, `target`/`reason` required |
| 8 | `/purge` | `TWO_MODERATION` | ManageMessages | Ephemeral outcome + message delete | `GuildRestricted`, `ModerationDisabled`, `ModerationPermission(Purge)`, `count` 1–100, `reason` required ≤512 |
| 9 | `/slowmode` | `TWO_MODERATION` | ManageChannels | Ephemeral outcome + slowmode effect (0 disables) | `GuildRestricted`, `ModerationDisabled`, `ModerationPermission(Slowmode)`, `seconds` 0–21600, `reason` required ≤512 |
| 10 | `/lockdown` | `TWO_MODERATION` | ManageChannels | Ephemeral outcome + send block | `GuildRestricted`, `ModerationDisabled`, `ModerationPermission(Lockdown)`, `reason` required ≤512 |
| 11 | `/unlock` | `TWO_MODERATION` | ManageChannels | Ephemeral outcome + send restore | `GuildRestricted`, `ModerationDisabled`, `ModerationPermission(Unlock)`, `reason` required ≤512 |
| 35 | `/purge-preview` | `TWO_MODERATION` | ManageMessages (8192) | Deferred ephemeral sampled counts, candidate time span and exclusions; no delete, audit, claim or lane writes; explicitly sampling, not a guarantee | `GuildRestricted`, `ModerationDisabled`, `PurgePreviewPermission`, only required `count` 1–100; no reason |
| 36 | `/channel-inspect` | `TWO_MODERATION` | ManageChannels (16) | Deferred ephemeral saved lockdown and live slowmode; absent lockdown is none, unavailable metadata is unknown; no config, audit, claim or lane writes | `GuildRestricted`, `ModerationDisabled`, `ChannelInspectPermission`, no options |
| 47 | `/moderation-recent` | `TWO_MODERATION` | ModerateMembers (1099511627776) | Deferred ephemeral newest-first page of saved moderation ledger rows labelled not live-verified; actor's Moderate Members standing rechecked live; applied, denied and uncertain outcomes stay distinct; no claim, audit, retry or Discord mutation | `GuildRestricted`, `ModerationDisabled`, `ModerationRecentPermission`, optional `page` 1–20 only; duplicate, unknown or wrong-typed options refuse before any read; empty, past-end and store-failure replies are finite and private |

Member-moderation policy order (ban, tempban, kick, timeout, warn):
permission → target present → self-target → owner/Owen/bot/staff protection →
bot hierarchy → actor hierarchy. Audit `reason` is trimmed, non-empty, ≤512
UTF-16 units on every mutating moderation row. The four private reads take no
reason and write no audit record. Failed reads stay private with retry guidance;
unknown metadata never becomes a claim that no restriction exists.

## Voice slice: vote-kick, setup/import (CI-proven handlers; staging round-trip UNPROVEN)

The `TWO_VOICE` slice stays out of the live 47-row suite above. Its handlers
are proven deterministically in CI with constructed interaction inputs and no
guild, network, or database. The user-originated staging round-trip is
UNPROVEN: a bot token cannot invoke slash commands or click components, so no
unattended synthetic can drive it. These rows need a supervised human tester
run on staging. Never record them as passed until a human has run them.

| Command | CI proof (deterministic, no guild) | Staging expectation (human run) | Staging status |
| --- | --- | --- | --- |
| `/kick` (vote) | `vote_kick_open_vote_close_round_trip_through_interactions`: occupant opens via `/kick`, Yes ballots to strict majority close Passed with disabled buttons, replay stays terminal; room-scoped deny/disconnect enforcement pinned by `strict_majority_passes_and_enforces_on_that_room_only` | Occupant `/kick` opens a public ballot; majority Yes disconnects and Connect-denies the target on that room only; refusals stay ephemeral with no state change | UNPROVEN — human run required |
| `/setup` | `setup_then_import_confirm_round_trip_through_interactions` reads the admin panel before and after the import | Admin sees status/health/creators; members see the generic panel only | UNPROVEN — human run required |
| `/import` | Same test: upload previews the diff, Confirm applies the file exactly once (`Import applied: …`), stored config equals the upload, replayed Confirm expires with no second apply; Cancel-consumes-preview pinned by `import_cancel_writes_nothing_and_consumes_the_preview` | Manage Server upload shows the diff preview; Confirm applies it; Cancel writes nothing | UNPROVEN — human run required |

Out of this slice: owner-command round-trips (`/limit`, `/private`, `/name`)
ship separately and are untouched here.

## Out of scope

- Voice `/create /setup /ping /invite /textchannels /access /reclaim
  /transfer /logging /export /import /position /group /inheritpermissions
  /defaultlimit /alwaysprivate /kick /name /private /public /limit /unlimit`
  (separate `TWO_VOICE` slice; its
  `kick` loses the merge to moderation first-wins — runtime dispatch decides).
  The published voice set is pinned separately by the `voice` section of
  `crates/core/tests/fixtures/staging_published_commands.json`.
- `/templateassistant` (unpublished until its slice lands; name still reserved, same fixture section).
- DB-backed custom commands and `!` prefix triggers (dynamic surfaces).
- Component/modal surfaces (pickers, tickets, self-roles, LFG signup).
- Dropped `/rota-acknowledge` (parity row 13, never published).

## Live-suite follow-up

The live run belongs to the parent staging-guild smoke epic: run each row's
success probe plus one denial probe per gate/permission group against the
staging guild, and file results there. Safe denial probes (unknown name, gate
off, permission denied, invalid option) must assert the exact refusal text and
no watch-log row; only the store-unavailable path may file `store_unavailable`.
