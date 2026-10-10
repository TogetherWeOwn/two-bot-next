# Voice-room presence facts (`TWO_VOICE_PRESENCE`)

Room-name templates can show what members are doing: `@@game_name@@`,
`@@num_playing@@`, `@@num_live@@`, `@@stream_name@@`, the party tokens, and
the `PLAYING`, `LIVE`, `LIVE_DISCORD`, `LIVE_EXTERNAL`, `ANY_LIVE`, `GAME`,
`PLAYERS`, `RICH` and `MAX` conditions (`docs/voice-rooms.md` §V5/§V6). Those
inputs come from member presences, which Discord only sends with the
privileged `GUILD_PRESENCES` gateway intent.

## Switch

| Variable | Default | Effect |
| --- | --- | --- |
| `TWO_VOICE_PRESENCE` | off | `1` (exact) together with `TWO_VOICE=1` requests `GUILD_PRESENCES`. |

Turn on the application's **Presence Intent** in the Discord Developer Portal
first (`GET /applications/@me` shows the `GATEWAY_PRESENCE` or
`GATEWAY_PRESENCE_LIMITED` flag). Requesting the intent without that grant
makes Discord close the gateway with 4014, so the switch stays off by default
and a bot without the grant always starts.

## What is read

`crates/core/src/voice_presence.rs` reduces each member's activities to the
first game (with its party size and text) and the first external stream
(Twitch, YouTube), bounded to 128 characters per text. Streaming through
Discord comes from the member's voice state (`self_stream`), which needs no
privileged intent. Nothing is persisted: facts live in the guild's in-memory
voice snapshot, only for members with a game or stream, capped at 100,000
members.

Game titles pass through the guild's `/alias` table, then the majority rule
(`resolve_majority_game`) with the guild's force-single-game and
count-members-without-activity settings, so `@@game_name@@` and the `GAME`
condition agree.

## Re-rendering

A game or stream change for a member who sits in a tracked room clears that
room's last render signature; the template-name tick re-renders it through the
rename coalescer, inside Discord's two-renames-per-ten-minutes budget. The
signature includes the game title, player and live counts, the stream title
and the owner's playing/live flags, so unrelated presence churn (status text,
music) never renames a room.
