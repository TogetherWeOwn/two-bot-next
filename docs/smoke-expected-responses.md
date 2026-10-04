# Smoke expected responses (offline table)

Offline smoke-prep reference, derived from code only. No staging guild, no
live Discord, no secrets, no network, no database. The live-guild pass is
separate follow-up work; this document never touches staging.

The five commands are one per routing family, so every fence, gate, and
permission branch the smoke harness can hit is covered. The set is pinned by
the offline fixture test alongside the router.

## Expected-response table

| Command | Happy-path reply shape | Denied-path copy (pointer / TBD) |
| --- | --- | --- |
| `/rank` | Ephemeral reply built by `rank_reply`: `content` is `rank_text`, visibility ephemeral (`crates/core/src/leveling.rs:150`, `crates/core/src/leveling.rs:251`). Ranked shape: `**{name}**`, `Level **{level}** · Rank **#{rank}** of **{members}**`, `XP **{xp}** · {progress}/{span} this level · **{to_next}** to level {next}`. Unranked shape replaces the rank clause with `Rank **Unranked** (no XP recorded)`. Numbers group with commas. | Core is always on: stays live while feature gates are off. Foreign or missing guild is fenced silently (`Ignore`). Unknown names get the uniform unknown-command reply (`crates/core/src/router/replies.rs:11`). Handler errors get the generic correlation reply (see shared copy below). Live-guild TBD: confirm the exact rendered rank line for a seeded profile. |
| `/leaderboard` | Public reply built by `leaderboard_reply`: `content` is `leaderboard_text`, mentions suppressed (`crates/core/src/leveling.rs:192`, `crates/core/src/leveling.rs:277`). Populated shape: `**TWO XP Leaderboard**` header plus one row per entry, `**{rank}.** <@{member_id}> · level **{level}** · {xp} XP` (`crates/core/src/leveling.rs:178`). Empty board shape: `No XP has been earned yet.` | Same fence as `/rank`: always on, foreign or missing guild is `Ignore`, unknown names get the uniform unknown-command reply. Live-guild TBD: confirm top-10 order and that no row pings (mention parse is empty). |
| `/rsvp` | Ephemeral reply `RSVP saved: {status}.` where status is `going`, `interested`, or `declined` (`crates/core/src/rsvp.rs:177`). Open to everyone: no permission bits required. | Announcements gate off refuses with the announcements-disabled copy (`crates/core/src/router.rs:96`). Foreign or missing guild is `Ignore`. Live-guild TBD: confirm the saved-status echo for each of the three statuses. |
| `/lfg` | Ephemeral reply ``LFG posted: `{post_id}`.`` (`crates/core/src/lfg.rs:543`). The posted board content shape is `lfg_content` (`crates/core/src/lfg.rs:423`); signup and leave replies are `LFG {outcome}.` / `LFG left.` (`crates/core/src/lfg.rs:549`). Requires Manage Events. | Announcements gate off refuses with the announcements-disabled copy (`crates/core/src/router.rs:96`). Missing Manage Events refuses with the manage-events copy (`crates/core/src/router.rs:91`). Foreign or missing guild is `Ignore`. Live-guild TBD: confirm the posted board renders title, slots, and signups. |
| `/ban` | Routes to the moderation ban handler; the executor performs the REST membership effect from the `Banned { user_id }` outcome (`crates/core/src/action_outcomes.rs:36`). User-facing success confirmation copy is TBD in the live-guild pass — code pins routing and the REST effect, not the exact confirmation sentence, so this table does not assert one. Requires Ban Members. | Moderation gate off refuses with the moderation-disabled copy (`crates/core/src/router.rs:98`). Missing Ban Members refuses with `You need the Ban Members permission to use /ban. Ask a server moderator or admin to grant it.` (`crates/core/src/router.rs:262`, permission name from `crates/core/src/moderation.rs:104`). Outside the configured guild refuses with the guild-restricted copy (`crates/core/src/router.rs:94`). Live-guild TBD: record the exact confirmation copy without pasting member data. |

## Shared denied and error copy

All refusals answer as an ephemeral type 4 callback with mentions suppressed
(`crates/discord/src/interactions.rs:142`).

- Unknown slash name: `I don't recognize that command. It may have been
  removed or renamed — pick it again from the / command list.`
  (`crates/core/src/router/replies.rs:11`).
- Stale button or menu: `That button or menu has expired. Run the command
  again to get a fresh one.` (`crates/core/src/router/replies.rs:14`).
- Automations off: `Automations are disabled on this server. Ask a server
  admin to enable them in the bot configuration — this is a host setting, not
  a Discord role.` (`crates/core/src/router.rs:86`).
- Announcements off: `Announcements are disabled on this server. Ask a server
  admin to enable them in the bot configuration — this is a host setting, not
  a Discord role.` (`crates/core/src/router.rs:96`).
- Moderation off: `Moderation is not enabled on this server. Ask a server
  admin to enable it in the bot configuration — this is a host setting, not
  a Discord role.` (`crates/core/src/router.rs:98`).
- Missing Manage Events: `You need the Manage Events permission to use this
  command. Ask a server admin to grant it.` (`crates/core/src/router.rs:91`).
- Guild fence (moderation only): `This command is restricted to the
  configured guild.` (`crates/core/src/router.rs:94`). All other families
  answer foreign or missing guilds with silence (`Ignore`), not a reply.
- Handler error or panic: `Something went wrong (ref XXXXXXXX). Please try
  again — if it keeps happening, share this reference with a server admin.`
  The eight hex digits are random per failure; see the reply and error
  contract in `docs/interaction-replies.md`.

## How the live-guild pass uses this table

1. For each command, send the happy-path input and compare the reply shape
   against the left column (exact text for `/rsvp` and `/lfg` creation;
   shape match for `/rank` and `/leaderboard`; recorded copy for `/ban`).
2. For each gated command, repeat with the feature off and with a member
   missing the required permission, and compare against the right column.
3. Record only verdicts and shape tokens, never member data, tokens, or
   guild internals.

## Sources

- The five-set and its routing pins:
  `crates/discord/tests/top5_reply_fixtures.rs:18`,
  `crates/discord/tests/top5_reply_fixtures.rs:166`.
- Reply lifecycle and visibility rules: `docs/interaction-replies.md`.
- Router gates, refusals, and guild fence: `crates/core/src/router.rs:80`,
  `crates/core/src/router.rs:234`.
- Refusal serialization: `crates/discord/src/interactions.rs:147`.
- Leveling runtime ownership of the immediate rank/leaderboard callback:
  `crates/bot/src/command_runtime.rs:831`.
