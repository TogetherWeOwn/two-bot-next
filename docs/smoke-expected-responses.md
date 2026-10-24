# Smoke expected responses (offline table)

Offline smoke-prep reference, derived from code only. No staging guild, no
live Discord, no secrets, no network, no database. The live-guild pass is
separate follow-up work; this document never touches staging.

The five commands are one per routing family, so every fence, gate, and
permission branch the smoke harness can hit is covered. The set is pinned by
the offline fixture test alongside the router. The rows below the five extend
the same table to `/help`, `/privacy`, and the remaining moderation verbs, so
the whole smoke-relevant surface reads from one place.

## Expected-response table

| Command | Happy-path reply shape | Denied-path copy (pointer / TBD) |
| --- | --- | --- |
| `/rank` | Ephemeral defer before the store read, completed by an edit built by `rank_reply`: `content` is `rank_text`, visibility ephemeral (`crates/core/src/leveling.rs:150`, `crates/core/src/leveling.rs:251`). Ranked shape: `**{name}**`, `Level **{level}** · Rank **#{rank}** of **{members}**`, `XP **{xp}** · {progress}/{span} this level · **{to_next}** to level {next}`. Unranked shape replaces the rank clause with `Rank **Unranked** (no XP recorded)`. Numbers group with commas. | Core is always on: stays live while feature gates are off. Foreign or missing guild is fenced silently (`Ignore`). Unknown names get the uniform unknown-command reply (`crates/core/src/router/replies.rs:11`). A store-read failure after the defer edits the original reply with `Rank is unavailable right now; try again.` and suppressed mentions; the store error propagates for logging, without a second reply or the shared generic correlation copy. Live-guild TBD: confirm the exact rendered rank line for a seeded profile. |
| `/leaderboard` | Public defer before the store read, completed by an edit built by `leaderboard_reply_for_season`: `content` is the seasonal header `**TWO XP Leaderboard ({season})**` plus body, mentions suppressed (`crates/core/src/leveling.rs:478`). The 2026-Q4 header carries ` — partial: early-season XP not counted` inside the same header line; later quarters render the plain header. One row per entry, `**{rank}.** <@{member_id}> · level **{level}** · {xp} XP`, where level and XP derive from that season's `seasonal_xp` (`crates/core/src/leveling.rs:364`, `crates/core/src/leveling_store.rs:715`). An empty season renders the header plus `No XP has been earned yet.` with no lifetime fallback. | Same fence as `/rank`: always on, foreign or missing guild is `Ignore`, unknown names get the uniform unknown-command reply. A store-read failure after the defer edits the original reply with `Leaderboard is unavailable right now; try again.` and suppressed mentions; the store error propagates for logging, without a second reply. Live-guild TBD: confirm top-10 order and that no row pings (mention parse is empty). |
| `/rsvp` | Ephemeral reply `RSVP saved: {status}.` where status is `going`, `interested`, or `declined` (`rsvp_saved_text`, `crates/core/src/rsvp.rs:192`). Open to everyone: no permission bits required. | Announcements gate off refuses with the announcements-disabled copy (`crates/core/src/router.rs:106`). Foreign or missing guild is `Ignore`. Live-guild TBD: confirm the saved-status echo for each of the three statuses. |
| `/lfg` | Ephemeral reply ``LFG posted: `{post_id}`.`` (`created_reply`, `crates/core/src/lfg.rs:557`). The posted board content shape is `lfg_content` (`crates/core/src/lfg.rs:437`); signup and leave replies are `LFG {outcome}.` / `LFG left.` (`signup_reply`, `crates/core/src/lfg.rs:563`; `leave_reply`, `crates/core/src/lfg.rs:570`). Requires Manage Events. | Announcements gate off refuses with the announcements-disabled copy (`crates/core/src/router.rs:106`). Missing Manage Events refuses with the manage-events copy (`crates/core/src/router.rs:101`). Foreign or missing guild is `Ignore`. Live-guild TBD: confirm the posted board renders title, slots, and signups. |
| `/ban` | Ephemeral reply `Banned {target}.` built by `outcome_text_with_target` (`MemberOutcome::Banned` arm, `crates/bot/src/member_runtime.rs`); the slot names the resolved display name (user id fallback), sanitized (single-line, mentions neutralized, pills split, 512-cap). The executor performs the REST membership effect from the `Banned { user_id }` outcome (`crates/core/src/action_outcomes.rs:36`) via `execute_outcome` (`crates/discord/src/executor.rs:3481`). Replayed bans append ` Already applied.` after the target. Pinned by `ban_success_confirmation_copy_is_pinned` (`crates/bot/src/member_runtime.rs`). Requires Ban Members. | Moderation gate off refuses with the moderation-disabled copy (`crates/core/src/router.rs:108`). Missing Ban Members refuses with `You need the Ban Members permission to use /ban. Ask a server moderator or admin to grant it.` (`crates/core/src/router.rs:287`, permission name from `crates/core/src/moderation.rs:104`). Outside the configured guild refuses with the guild-restricted copy (`crates/core/src/router.rs:104`). Code-pinned copy is `Banned {target}.` (replay: `Banned {target}. Already applied.`); live-guild pass records only verdict/shape, never member data. |
| `/help` | Immediate ephemeral reply built by `help_text` (`crates/core/src/help.rs`) via `help_response` (`crates/discord/src/interactions.rs`): header `**Server commands** (N live)` plus one section per live group in publish order (`HELP_GROUPS`), gated groups carrying `— needs <permission>` hints and open groups carrying none. Names render as `/name · …` tokens; large registries keep the same text in an embed instead of truncating. Answered from the last confirmed publication by the `HandlerId::Help` branch (`crates/bot/src/command_runtime.rs`); with no confirmed publication yet the reply is `The command list is still refreshing. Try /help again shortly.` Open to everyone: no permission bits required. | Always on (`RowGate::Always`, `permission_refusal: None`, `crates/core/src/router.rs`): no feature gate refuses it. Foreign or missing guild is fenced silently (`Ignore`). Unknown names get the uniform unknown-command reply. Live-guild TBD: confirm the header count matches the published set and every live name appears exactly once. |
| `/privacy` | Ephemeral completion of the deferred reply by `privacy_command` (`crates/bot/src/privacy_command.rs`); the subcommand is decoded by `privacy_action` (`opt-out` / `opt-in` / `status` / `delete` / `deletion-status` / `deletion-receipt`, anything else is `Invalid`). The deletion request passes only the interaction guild, `actor_id(interaction)`, and the interaction id to the intake; status and receipt pass the same authenticated pair plus the requested id. Fresh requests acknowledge pending; a same-interaction retry reuses the request and directs the member to status instead of claiming a new pending queue. Fresh opt-out: ``You are opted out of analytics for this server. Your stored activity here was deleted and future activity will not be stored. Moderation, safety and delivery records are unchanged. Use `/privacy opt-in` to resume.`` Repeat opt-out gets the already-opted-out reply, rapid re-taps get the cooldown reply (`ALREADY_OPTED_OUT_REPLY`, `OPT_OUT_COOLDOWN_REPLY`). Opt-in answers `Analytics persistence is back on for this server. Rows deleted while you were opted out are not restored.` (or the already-on variant); status answers `Analytics for you in this server: <opted out\|on>. Server switches: presence <on\|off>, activity <on\|off>, voice <on\|off>.` The subject is always the invoker: the definition carries no target option. | Always on, open to everyone (`RowGate::Always`, `permission_refusal: None`, `crates/core/src/router.rs`). Guild-less and foreign-guild invocations get no reply on the normal slash path; the router returns `Ignore` (the guild-only reply `GUILD_ONLY_REPLY` is reachable only by a direct `privacy_command` handler call); an unidentifiable invoker gets the unknown-member reply and store failures get `Privacy command failed; try again.` (`STORE_FAILURE_REPLY`); a non-subcommand invocation gets the usage reply (`USAGE_REPLY`). Live-guild TBD: confirm the opt-out / opt-in / status echoes word for word. |
| `/kick` | Ephemeral reply `Kicked {target}.` built by `outcome_text_with_target` (`MemberOutcome::Kicked` arm, `crates/bot/src/member_runtime.rs`), delivered by `handle_command` editing the defer; the slot names the resolved display name (user id fallback), sanitized. Replayed kicks append ` Already applied.` after the target (`MemberResult.replayed`). Requires Kick Members (`ModerationAction::Kick`, `discord_permission_name`, `crates/core/src/moderation.rs`). | Moderation gate off refuses with the moderation-disabled copy (`MODERATION_DISABLED_REPLY`, `crates/core/src/router.rs`). Missing Kick Members refuses with `You need the Kick Members permission to use /kick. Ask a server moderator or admin to grant it.` (`RouterRefusal::ModerationPermission`). Outside the configured guild refuses with the guild-restricted copy (`GUILD_RESTRICTED_REPLY`). Protected, self, and hierarchy targets refuse with the `PolicyError` text via `error_text` (`This target cannot be moderated`, the hierarchy lines). A missing `target` (like any other malformed option) refuses earlier at strict decode with `Cannot run /kick: ... Try: /kick target:@user reason:text`, before any read, cooldown or audit. With voice on, when the invoker and target share a tracked voice room, the moderation-disabled and missing-permission refusals above are replaced by the room-vote ballot (`start_kick_vote`, `crates/bot/src/voice_rooms.rs`); test that refusal copy outside a shared voice room. Live-guild TBD: confirm verdict/shape only, never member data. |
| `/timeout` | Ephemeral reply `Timed out {target}.` built by `outcome_text_with_target` (`MemberOutcome::TimedOut` arm, `crates/bot/src/member_runtime.rs`), delivered by `handle_command` editing the defer. Replayed timeouts append ` Already applied.` after the target; the slot names the resolved display name (user id fallback), sanitized. Requires Moderate Members (`ModerationAction::Timeout`, `discord_permission_name`, `crates/core/src/moderation.rs`). The `duration_seconds` option is required and bounded 60–2,419,200 by both the definition and `validate_timeout_duration`. | Same three refusals as `/kick`: moderation-disabled copy when the gate is off, `You need the Moderate Members permission to use /timeout. Ask a server moderator or admin to grant it.` when the permission is missing, guild-restricted copy outside the configured guild. Wrong-typed, duplicated, missing, unknown or out-of-range options refuse at strict decode, before any read, cooldown or audit, with `Cannot run /timeout: ... Try: /timeout target:@user reason:text duration_seconds:60`. Protected, self, and hierarchy targets refuse with the `PolicyError` text. Live-guild TBD: verdict/shape only. |
| `/tempban` | Ephemeral reply `Temporarily banned {target}.` built by `outcome_text_with_target` (`MemberOutcome::TemporarilyBanned` arm, `crates/bot/src/member_runtime.rs`), delivered by `handle_command` editing the defer. Replayed tempbans append ` Already applied.` after the target; the slot names the resolved display name (user id fallback), sanitized. Requires Ban Members (`ModerationAction::TempBan`, `discord_permission_name`, `crates/core/src/moderation.rs`). The `duration_seconds` option is required and bounded 60–31,536,000 by both the definition and `validate_tempban_duration`. | Same three refusals as `/kick`: moderation-disabled copy when the gate is off, `You need the Ban Members permission to use /tempban. Ask a server moderator or admin to grant it.` when the permission is missing, guild-restricted copy outside the configured guild. Wrong-typed, duplicated, missing, unknown or out-of-range options refuse at strict decode, before any read, cooldown or audit, with `Cannot run /tempban: ... Try: /tempban target:@user reason:text duration_seconds:60`. Protected, self, and hierarchy targets refuse with the `PolicyError` text. Live-guild TBD: verdict/shape only. |
| `/warn` | Ephemeral reply `Warned {target}.` built by `outcome_text_with_target` (`MemberOutcome::Warned` arm, `crates/bot/src/member_runtime.rs`), delivered by `handle_command` editing the defer. Replayed warnings append ` Already applied.` after the target; the slot names the resolved display name (user id fallback), sanitized. Requires Moderate Members (`ModerationAction::Warn`, `discord_permission_name`, `crates/core/src/moderation.rs`). | Same three refusals as `/kick`: moderation-disabled copy when the gate is off, `You need the Moderate Members permission to use /warn. Ask a server moderator or admin to grant it.` when the permission is missing, guild-restricted copy outside the configured guild. Protected, self, and hierarchy targets refuse with the `PolicyError` text via `error_text`. A missing `target` (like any other malformed option) refuses earlier at strict decode with `Cannot run /warn: ... Try: /warn target:@user reason:text`, before any read, cooldown or audit. Live-guild TBD: verdict/shape only, never member data. |
| `/purge` | Full completion: ephemeral reply `Moderation action completed: purged (N).` Early stop after confirmed deletions: `Moderation action completed: purged (N, stopped early: refused).` or `Moderation action completed: purged (N, stopped early: rate_limited).` (`rate_limited` for 429, `refused` for other definitive refusals; no retry/resume). The count stays the number actually deleted. These replies are built by `moderation_result_text` (`ChannelOutcome::Purged` arm, `crates/core/src/channel_moderation.rs`), served by `ChannelModerationRuntime` (`crates/bot/src/command_runtime.rs`). Requires Manage Messages (`ModerationAction::Purge`, `discord_permission_name`, `crates/core/src/moderation.rs`). The `count` option is required, 1–100. Channel verbs skip member-target checks (`targets_member()` is false). | Moderation gate off refuses with the moderation-disabled copy (`MODERATION_DISABLED_REPLY`). Missing Manage Messages refuses with `You need the Manage Messages permission to use /purge. Ask a server moderator or admin to grant it.` (`RouterRefusal::ModerationPermission`). Outside the configured guild refuses with the guild-restricted copy. Live-guild TBD: confirm the `purged (N)` count echo and verdict only. |
| `/slowmode` | Ephemeral reply `Moderation action completed: slowmode_updated.` built by `moderation_result_text` (`ChannelOutcome::SlowmodeUpdated` arm, `crates/core/src/channel_moderation.rs`), served by `ChannelModerationRuntime`. Requires Manage Channels (`ModerationAction::Slowmode`). The `seconds` option is required, 0–21600, where 0 disables (`validate_slowmode_seconds`). | Same moderation triple: moderation-disabled copy when the gate is off, `You need the Manage Channels permission to use /slowmode. Ask a server moderator or admin to grant it.` when the permission is missing, guild-restricted copy outside the configured guild. Out-of-range seconds refuse with the bounds text (`BoundsError`). Live-guild TBD: verdict/shape only. |
| `/lockdown` | Ephemeral reply `Moderation action completed: locked_down.` built by `moderation_result_text` (`ChannelOutcome::LockedDown` arm, `crates/core/src/channel_moderation.rs`), served by `ChannelModerationRuntime`. Requires Manage Channels (`ModerationAction::Lockdown`). Planning denies SEND_MESSAGES on the @everyone overwrite via `plan_lockdown`; the seed persists before the overwrite applies. | Same moderation triple: moderation-disabled copy when the gate is off, `You need the Manage Channels permission to use /lockdown. Ask a server moderator or admin to grant it.` when the permission is missing, guild-restricted copy outside the configured guild. Live-guild TBD: verdict/shape only. |
| `/unlock` | Ephemeral reply `Moderation action completed: unlocked.` built by `moderation_result_text` (`ChannelOutcome::Unlocked` arm, `crates/core/src/channel_moderation.rs`), served by `ChannelModerationRuntime`. Requires Manage Channels (`ModerationAction::Unlock`). Undo restores only the recorded lockdown change via `plan_unlock`; unlocking with no recorded lockdown refuses (`UnlockError::NotLocked`). | Same moderation triple: moderation-disabled copy when the gate is off, `You need the Manage Channels permission to use /unlock. Ask a server moderator or admin to grant it.` when the permission is missing, guild-restricted copy outside the configured guild. Live-guild TBD: verdict/shape only. |

## Shared denied and error copy

All router-answered refusals answer as an ephemeral type 4 callback with mentions suppressed
(`text_response`, `crates/discord/src/interactions.rs:115`). Channel verbs
(`/purge`, `/slowmode`, `/lockdown`, `/unlock`) answer every outcome,
refusals included, as an ephemeral deferred reply completed by an edit
(`ChannelModerationRuntime::respond`, `crates/discord/src/channel_moderation.rs`).

- Unknown slash name: `I don't recognize that command. It may have been
  removed or renamed — pick it again from the / command list.`
  (`crates/core/src/router/replies.rs:11`).
- Stale button or menu: `That button or menu has expired. Run the command
  again to get a fresh one.` (`crates/core/src/router/replies.rs:14`).
- Automations off: `Automations are disabled on this server. Ask a server
  admin to enable them in the bot configuration — this is a host setting, not
  a Discord role.` (`crates/core/src/router.rs:96`).
- Announcements off: `Announcements are disabled on this server. Ask a server
  admin to enable them in the bot configuration — this is a host setting, not
  a Discord role.` (`crates/core/src/router.rs:106`).
- Moderation off: `Moderation is not enabled on this server. Ask a server
  admin to enable it in the bot configuration — this is a host setting, not
  a Discord role.` (`crates/core/src/router.rs:108`).
- Missing Manage Events: `You need the Manage Events permission to use this
  command. Ask a server admin to grant it.` (`crates/core/src/router.rs:101`).
- Guild fence (moderation only): `This command is restricted to the
  configured guild.` (`crates/core/src/router.rs:104`). All other families
  answer foreign or missing guilds with silence (`Ignore`), not a reply.
- Handler error or panic on the shared error-reply path: `Something went
  wrong (ref XXXXXXXX). Please try again — if it keeps happening, share this
  reference with a server admin.` The eight hex digits are random per
  failure; see the reply and error contract in `docs/interaction-replies.md`.
  This copy does not apply to acknowledged `/rank` or `/leaderboard` store
  failures: their runtime owns the fixed unavailable edit above, while the
  caller only logs the error.

## How the live-guild pass uses this table

1. For each command, send the happy-path input and compare the reply shape
   against the left column (exact text for `/rsvp`, `/lfg` creation,
   `/ban`, `/kick`, `/timeout`, `/tempban`, `/warn`, `/purge`,
   `/slowmode`, `/lockdown`, `/unlock`, and the `/privacy` subcommand
   echoes; shape match for `/rank`, `/leaderboard`, and `/help`).
2. For each gated command, repeat with the feature off and with a member
   missing the required permission, and compare against the right column.
3. Record only verdicts and shape tokens, never member data, tokens, or
   guild internals.

## Sources

- The five-set and its routing pins: `TOP_FIVE`
  (`crates/discord/tests/top5_reply_fixtures.rs:170`) and
  `top_five_route_to_their_handlers_with_no_router_owned_reply`
  (`crates/discord/tests/top5_reply_fixtures.rs:200`).
- Reply lifecycle and visibility rules: `docs/interaction-replies.md`.
- Router gates, refusals, and guild fence: `RouterGates`
  (`crates/core/src/router.rs:166`), `RouterRefusal`
  (`crates/core/src/router.rs:261`), `route_slash`
  (`crates/core/src/router.rs:401`).
- Refusal serialization: `refusal_response`
  (`crates/discord/src/interactions.rs:183`).
- Leveling runtime ownership of the defer-then-edit rank/leaderboard replies:
  the `HandlerId::Rank | HandlerId::Leaderboard` branch
  (`crates/bot/src/command_runtime.rs:1429-1441`).
- Discovery reply: `help_text` and `HELP_GROUPS`
  (`crates/core/src/help.rs`), served as one immediate ephemeral reply by
  `help_response` (`crates/discord/src/interactions.rs`) from the
  `HandlerId::Help` branch (`crates/bot/src/command_runtime.rs`).
- Privacy self-serve: `privacy_command` and `privacy_action`
  (`crates/bot/src/privacy_command.rs`); always on and open to everyone
  (`RowGate::Always`, `crates/core/src/router.rs`).
- Member moderation replies: `outcome_text_with_target`, `target_display`,
  and `handle_command` (`crates/bot/src/member_runtime.rs`), pinned for
  `/ban` by
  `ban_success_confirmation_copy_is_pinned`; policy refusals via
  `PolicyError` and `error_text`; permission names via
  `ModerationAction::discord_permission_name`
  (`crates/core/src/moderation.rs`).
- Channel moderation replies: `moderation_result_text`, `ChannelOutcome`,
  `plan_lockdown`, `plan_unlock`, and `validate_slowmode_seconds`
  (`crates/core/src/channel_moderation.rs`), served by
  `ChannelModerationRuntime` (`crates/bot/src/command_runtime.rs`);
  result text pinned by `result_text_matches_legacy_format`.
- Moderation routing triple: `MODERATION_DISABLED_REPLY`,
  `GUILD_RESTRICTED_REPLY`, and
  `RouterRefusal::ModerationPermission` (`crates/core/src/router.rs`).
