# Voice rooms: functional specification

Owner priority (2026-09-29): full temporary voice-room support in two-bot-next,
built from this behaviour spec. It is an original implementation: write it from
this document only, and don't copy code from any other project.

Commands are slash commands with ephemeral replies. "Admin" means the guild owner
or a member with guild-level Manage Channels or Administrator role permissions,
unless a row says otherwise. Interaction `member.permissions` includes source-channel
overwrites and is never an admin credential: owning a room does not authorize
another room or guild-wide settings. Missing guild-role snapshots fail closed.
See [Discord's member definition](https://docs.discord.com/developers/resources/guild#guild-member-object)
and [base permission calculation](https://docs.discord.com/developers/topics/permissions#permission-hierarchy).
An active communication timeout removes role-derived Manage Channels and Manage
Server authority. Only existing View Channel and Read Message History permissions
remain; the guild owner and guild-level Administrator are exempt, as required by
[Discord's timeout rules](https://docs.discord.com/developers/topics/permissions#permissions-for-timed-out-members).
An absent or expired timeout (including expiry equal to the current time) changes
no authority. Timeout facts never replace missing guild/member/role evidence.
Room state and per-guild settings
are stored in Postgres (sqlx, bot migration range 0001–0999). Tests never touch
production; DB tests run on agent-testdb.

Slices V1–V12 below each map to one card. V5/V6 (template engine) are pure
library code with no Discord dependency and can start immediately.

## V1: Creator channels and room lifecycle

- Admins mark voice channels as **creator channels**. There can be any number per
  guild, and each has its own settings (template, permission source, default limit,
  privacy default, text-channel toggle, position).
- Joining a creator channel creates a new voice **room** and moves the member in.
  That member is the room's **owner** and **original creator**.
- New rooms copy bitrate, RTC region, video quality, NSFW flag and default user
  limit from their creator channel.
- Ordinary rooms are eligible for deletion after the empty grace (60 continuous
  human-empty seconds unless `TWO_TEMP_VOICE_EMPTY_GRACE_SECONDS` sets it; bots
  don't count; unknown bot identity counts as human). A human join
  cancels the deadline; a later leave starts a full grace, even between ticks.
  Reconnect snapshots start a fresh grace rather than counting disconnected time.
  If someone deletes a room by hand, the bot quietly forgets it.
- Configured Lobby, generator/creator and category IDs are never deleted, even
  when a room or companion provenance row claims them. Creator IDs loaded from
  the store and live category channels are protected too. Boot process inputs
  `DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID`, `TWO_TEMP_VOICE_GENERATOR_CHANNEL_ID`,
  `TWO_TEMP_VOICE_CATEGORY_ID` and comma-separated `TWO_TEMP_VOICE_PROTECTED_CHANNEL_IDS`
  supply additional protection; malformed IDs disable the voice runtime. Stored
  settings remain unwired. `TWO_TEMP_VOICE_EMPTY_GRACE_SECONDS` (0 to 600, read
  at boot) sets the empty grace; unset keeps 60 seconds. `0` deletes a room on
  the next tick after its last human leaves, matching the interim voice bot.
- If the member can't be moved in (missing Move Members, or they left first), the
  bot deletes the room immediately.
- If the bot loses access to a room (View Channel, Connect, Manage Channels or Move
  Members), it stops managing it without retry storms and resumes once access
  comes back.
- `/create`: makes a new creator channel.
- `/setup`: status panel with a guided walkthrough, health check, recent failures,
  the creator-channel list, a "New creator channel" quick action and "More
  settings". Anyone can view it; actions need admin.
- **Accept when:**
  - Each join produces exactly one room, the member ends up in it, and the room is
    deleted within a timer tick of the configured empty grace expiring.
  - Two members joining at the same moment get two rooms.
  - After a restart, the bot reconciles tracked rooms against the channels that
    actually exist and cleans up the empty ones.
  - A channel the bot never tracked is never deleted.
  - Hitting the 50-channels-per-category limit gives a clear error that suggests a
    second creator channel in another category.

## V2: Ownership

- When the owner leaves an occupied room, ownership passes to the longest-present
  member (the **caretaker**). The original creator is still remembered.
- `/reclaim`: the original creator takes ownership back while the caretaker is
  still there. It also lets a member claim a room whose owner has left.
- `/transfer member`: hands the room to a member who is in it. The recipient also
  becomes the original creator.
- Owner-only commands refuse everyone else. Guild admins may use owner commands
  in any tracked room; channel-scoped grants never provide that override.
- Succession, reclaim and transfer persist a room-scoped recipient journal before
  issuing any owner grant. The overwrite rewrite removes owner-grant bits from
  all recorded former recipients and installs the current owner's grant; unrelated
  overwrites and denies remain. Ownership SQL and revision-fenced cleanup
  acknowledgement commit atomically only after the rewrite is confirmed.
- Failed ownership SQL leaves cleanup pending across restart. Recovery converges
  to the persisted ledger owner even while that owner remains in the room; an
  uncommitted transfer is not promised after restart. Exhausted queue retries
  retain pending cleanup and replay in bounded cohorts with a one-minute cooldown,
  only with authoritative live evidence and restored overwrite access. Rapid
  handoffs coalesce per room without forgetting any issued recipient.
- Late overwrite responses cannot replace newer gateway channel updates/deletion
  or evidence from another connection generation. Follow-up rewrites preserve
  newly observed unrelated ACLs. Rooms created without Manage Roles stay
  category-synced rather than gaining a grant the bot cannot write.
- **Accept when:**
  - The caretaker is chosen by earliest join time.
  - `/transfer` rejects a target who is not in the room.
  - `/reclaim` is rejected for anyone who is not the original creator, unless the
    owner is absent.

## V3: Owner room controls

- `/limit count` (0 = unlimited, max 99). With no argument, the limit becomes the
  current headcount (a "lock"). `/unlimit` removes the limit.
- `/name`: a panel to set a custom name (template tokens allowed) or restore the
  template name.
  - Optional guild setting "unique names" rejects a literal name already used by
    another voice channel.
- `/private`: denies Connect to @everyone; the room stays visible.
  - A companion channel "⇩ Join ‹owner›" is created next to it.
  - When an outsider joins that channel, the owner gets **Approve / Deny / Block**
    buttons. Approve grants access and moves them in; Block stops further requests
    from that member.
- `/public`: restores access and deletes the Join channel.
- Per-user bitrate preference: must be above 8 kbps and at most the guild's tier
  maximum; can be reset. A room uses the average of its members' preferences,
  falling back to the creator channel's bitrate.
- **Accept when:**
  - Every reply is ephemeral with a clear success or error message.
  - Privacy survives ownership changes, and the Join channel follows the new
    owner's name.
  - The Join channel is deleted along with its room.

## V4: Vote-kick

- `/kick member [reason]`: any occupant can start a vote. The shared router
  answers every `/kick`. A moderator who passes the guild fence, the moderation
  gate and the Kick Members check always gets the moderation kick, so sitting in
  a room never shields a member from a moderator. Only a `/kick` the router
  refuses (moderation off, or the invoker lacks Kick Members) reaches the vote,
  and only when the invoker shares the target's room; everyone else gets the
  router's refusal.
- It passes with a strict majority of the occupants other than the target.
  Progress shows as required/total. Votes are cast with buttons; not voting
  counts as No. The vote expires after 2 minutes.
- If it passes, the target is disconnected and denied Connect on **that room
  only**.
- The public reason renders as bounded mention-safe plain text (gate VK-04):
  at most 512 characters (`VOTE_KICK_PUBLIC_REASON_LIMIT`), mass/user/role
  mentions and `<@`/`<#`/`<:`/`<a:` pills split so nothing pings, `://`,
  word-boundary `www.` (any case) and schemeless bare-domain dots split and
  inline markdown escaped so nothing is clickable, embedded or formatted, and
  `SUPPRESS_EMBEDS` set on the ballot. Refusals are fixed strings that never
  echo initiator text.
- The owner and original creator can't be targeted, and members can't target
  themselves. Targets with effective Kick Members or Administrator permission
  in the interaction's guild can't be targeted either; an unavailable
  guild-authority lookup fails closed with no vote. Only one active vote per
  target. If the target leaves, the vote is
  cancelled. Protection is rechecked on every ballot/refresh and again
  immediately before the disconnect/deny writes, so a promotion granted
  mid-vote produces no kick/disconnect effect.
- After a vote passes, expires or is cancelled, a fresh vote against the same
  member in the same guild is refused for 5 minutes (`Cooldown`,
  `VOTE_KICK_COOLDOWN_MS = 300_000`). The key is guild + target: a new
  interaction ID or a different room does not evade it. Expiry counts from the
  2-minute deadline even when observed late, so a slow timer cannot extend the
  cooldown.
- Each initiator can start at most 3 votes per 10 minutes per guild
  (`InitiatorLimited`, `VOTE_KICK_INITIATOR_LIMIT = 3`,
  `VOTE_KICK_INITIATOR_WINDOW_MS = 600_000`), across targets and rooms; other
  guilds are unaffected. Only successful starts count. Refused attempts create
  no vote, ballot or enforcement effect. Refusal precedence after target
  validation is: active vote, then target cooldown, then initiator cap.

## V5: Naming template engine (core library)

- One name template per creator channel. It can also be set on **standalone**
  permanent voice channels and on stage channels.
- A separate **voice status** template uses the same engine. Put fast-changing
  information there.
- Default name template: `@@random_emoji@@ @@owner@@'s [[den/crew/lair/hangout/base/club]]`.
- The name is recalculated on join or leave, activity changes, and limit or
  privacy changes.
- Evaluation order: conditionals (innermost first) → token substitution →
  styling → trim → truncate to 100 characters → fallback name if empty.

**Numbering**

| Token | Output |
|---|---|
| `##` | `#N` |
| `$#` | bare N |
| `$0#`, `$00#`, … | zero-padded (each extra 0 adds a digit) |
| `+#` | Roman numeral |
| `@@nato@@` | NATO alphabet word; after 26 it wraps and appends a cycle number |

- A room gets the lowest free number for its creator channel, or for its category
  when grouping is on (V8). The starting number is configurable.

**Plurals:** `<<singular/plural>>`

- Singular only when exactly one member is present.
- A `\` separator counts members excluding the owner.
- A `|` separator counts players in the largest rich-presence party.

**People and counts**

- `@@owner@@`: the member's display name, or their `/nick` name (V7).
  `@@creator@@` is an alias.
- `@@original_creator@@`
- `@@num@@`: humans in the room.
- `@@num_others@@`: humans excluding the owner.
- `@@num_live@@`: members streaming in Discord or externally.
- `@@limit@@`: the user limit, or 0 when unlimited.
- `@@slots@@`: free places left; blank when unlimited.

**Game, stream and party**

- `@@game_name@@`: the majority game, after aliases (V7).
  - A two-way tie shows both names.
  - Three or more tied shows the "no game" label (default `General`).
  - Optional setting: force a single game, preferring the owner's.
  - Optional setting: members with no visible activity count toward the majority
    game.
- `@@stream_name@@`: the owner's stream title while they are live.
- `@@num_playing@@`: the party size, otherwise the number of members playing.
- `@@party_size@@` (the party maximum, falling back to the limit),
  `@@party_state@@` and `@@party_details@@`: taken from the largest party; empty on
  a three-way tie.

**Time and random**

- `@@weekday@@`, `@@month@@`, `@@hour@@` (0–23): in the guild time zone (default
  UTC), English names.
- `@@daypart@@`: `morning` (05–11), `afternoon` (12–16), `evening` (17–21),
  `night` (22–01) or `late night` (02–04), local time.
- `@@room_minutes@@`: whole minutes since the room was created. `@@room_tier@@`:
  0 to 5 at 0, 15, 45, 90, 180 and 360 minutes.
- `@@game_minutes@@`: combined member-minutes the room has played the game it
  shows now (two players for ten minutes count twenty), kept in memory for the
  live room only and reset by a restart. `@@game_tier@@`: the same tiers.
- Time facts re-render a name only when a tier, the local hour or another fact
  changes, so a raw minute count shown in a name moves at those points, not every
  minute. The rename budget still applies.
- `@@random_emoji@@`, `[[a/b/c]]` and `[[list:name]]` (named lists from guild
  settings) are rolled from a per-room seed stored at creation and never re-rolled.

**Resting / in-use names**

- `__resting/in use__` applies to standalone channels only. It splits on the
  first `/` only, and tokens work on both sides.

**Accept when:**

- A golden corpus passes, for example `@@game_name@@ ##` → `Apex #3` and
  `@@nato@@ · @@num@@ <<person/people>>` → `Charlie · 4 people`.
- Output is never empty and never over 100 characters.
- Random picks stay stable across renames.
- Property tests pass for the parser.

## V6: Conditionals and styling

**Conditionals:** `{{cond ?? yes // no}}`

- `// no` is optional, and blocks nest.
- Comparisons `< > <= >= = !=` work on numbers and counter tokens (`@@num@@`,
  `@@limit@@`, `@@slots@@`, `@@hour@@`, `@@room_minutes@@`, `@@room_tier@@`,
  `@@game_minutes@@`, `@@game_tier@@`, `$#`), including token against token.

| Group | Keywords |
|---|---|
| Activity and streaming | `PLAYING`, `LIVE`, `LIVE_DISCORD`, `LIVE_EXTERNAL`, `ANY_LIVE` |
| Roles and people | `ROLE:id` (the owner has the role), `ANY_ROLE:id`, `MEMBER:id`, `OWNER:id`, bare `OWNER` |
| Game and party | `GAME` (`:` contains; `=` / `!=` exact), `PLAYERS`, `MAX`, `RICH` |
| Room state | `FULL`, `PRIVATE` |
| Date | `WEEKEND` (Saturday or Sunday), `WEEKDAY`, `MONTH` |
| Time of day | `MORNING`, `AFTERNOON`, `EVENING`, `NIGHT`, `LATE_NIGHT` (the `@@daypart@@` hours) |

- `FULL` requires a limit. `PRIVATE` is always false on standalone channels.
- An unknown condition is false.
- Name tokens are substituted after conditions resolve, so they never match inside
  a condition.
- `##` and `+#` are not numeric; comparisons use `$#`.

**Styling:** `""mode:text""`

- Modes chain with `+`. Unknown modes leave the text unchanged.
- Case: `upper`/`caps`, `lower`, `title`, `swap`, `scaps` (small caps; only
  lowercase letters convert), `rand` (random case, seeded per room).
- Words and spacing: `spaces`, `acro`, `remshort` (drops a, an, and, at, by, from,
  in, is, of, on, or, the, to), `<N>w` (first N words).
- Novelty: `uwu`, `usd` (upside down).
- Unicode fonts: `bold`, `italic`, `bolditalic`, `script`, `boldscript`, `fraktur`,
  `boldfraktur`, `double`, `sans`, `boldsans`, `italicsans`, `bolditalicsans`,
  `mono`.
- **Accept when:** every mode has unit tests, and nested fallback chains
  (role → live → default) resolve correctly.

## V7: Template admin, aliases, nicknames, inspection

- `/template`: a panel to edit the name and status templates for a creator or
  standalone channel. It previews before saving and flags invalid templates.
- `/alias`: a panel to add, edit or remove game-name aliases. Aliases apply to
  `@@game_name@@` and to `GAME` conditions.
- "More settings" in `/setup`: "no game" label, tied-games behaviour, time zone,
  named random lists, and text-channel naming and viewer role (V9).
- `/nick name|reset`: any member sets the name that `@@owner@@` shows for them.
- `/channelinfo [channel]` shows the owner, detected game, and why the room has
  its name. Buttons:
  - "All variables": every template variable with its current value.
  - "Preview in other states": nobody playing, full, locked, and so on.
  - Inspecting another channel needs admin.

## V8: Placement, permissions, per-creator defaults

- `/position`: new rooms go above or below the creator channel, and the admin sets
  the first room number. Existing rooms are not moved.
- `/group`: shared numbering and a contiguous block of rooms per category.
- `/inheritpermissions`: rooms copy overrides from the creator channel (default),
  its category, or a chosen channel. This needs Manage Roles; without it, rooms
  sync to their category.
- `/defaultlimit limit`: the starting limit for new rooms only.
- `/alwaysprivate`: new rooms from this creator start private.
- The owner gets extra permissions on their own room so they can manage it.
- Overrides are included when the channel is created, never patched afterwards.

## V9: Temporary text channels

- `/textchannels` is a per-creator toggle, off by default. When on, each room gets
  a companion text channel in the same category, deleted along with the room.
- Visible to current occupants (granted on join, removed on leave), admins, and
  one configurable role (can be @everyone).
- The name is configurable (default `voice-chat`).
- Changing the setting only affects text channels created afterwards.

## V10: Logging, health, errors, utilities

- `/logging`: log channel, detail level, who gets mentioned on errors, or off.
  Setting a channel requires a cached guild text channel where the invoking
  member can view and send messages. Setting a mention requires a cached,
  mentionable, unmanaged role other than @everyone, strictly below the member's
  highest role (the guild owner is exempt from hierarchy only). Mention Everyone
  does not bypass these checks. Missing cache evidence refuses the change;
  clearing either setting still works. Delivery suppresses stored roles that
  are deleted, managed or no longer mentionable, and skips configured channels
  that are no longer cached guild text channels.
- Error notices go to the first place that works: the guild system channel
  (mentioning whoever last set up the bot), a DM to that person or the guild
  owner, then the creator channel's chat.
- Notices repeat a few times, then stop. `/setup` always lists current failures.
- The health check looks for missing Manage Channels, Move Members, Manage Roles
  and View Channel at guild, category and channel level, and names a category
  override when that is the cause.
- Guild-level controls: turn room creation on or off (commands keep working); an
  optional role required to use room commands; per-command role restrictions, with
  admins always exempt.
- Utilities: `/ping` (latency) and `/invite`.

## V11: Configuration export and import

- `/export` (Manage Server): downloads the guild's voice configuration (creators,
  templates, aliases, lists, logging) as a versioned JSON file, ephemerally.
- `/import file` (Manage Server): shows a diff preview and confirms before writing.
  Unknown channel IDs are reported and skipped.

## V12: Template assistant (optional, config-gated)

- `/templateassistant` (admin): the admin describes the naming they want in plain
  language, in any language.
- The bot returns a template, a short explanation, and previews in six scenarios:
  solo with no game; three people in a game; owner streaming; a game with party
  info; nearly full with a limit; locked.
- The admin picks Apply, Refine or Cancel. Nothing changes until Apply.
- Every output is validated against all six scenarios (empty names, conditions
  that can never match, tokens that don't exist) and regenerated before the admin
  sees it if it fails.
- Tokens are always English. The explanation uses the language the admin asks
  for, otherwise their app locale.
- Only the request, the guild's templates, "no game" label and locale are sent. No
  member names, presence or IDs.
- Limits: 200 builds per guild per month, resetting on the 1st.
- Disabled unless an OpenAI-compatible endpoint is configured.

## Discord API notes (all slices)

- Channel **renames are limited to about 2 per 10 minutes per channel.**
  - Keep one pending name per channel, coalesce updates, and skip renames when the
    name hasn't changed.
  - Rename backlogs must never delay creating or deleting rooms.
  - Voice-status updates are much less limited, so route fast-changing information
    there.
- Create the channel with its overrides already included, then move the member.
  If the member left before the move, delete the room.
- Handle channel create, move and delete events idempotently.
  - Use per-guild ordered queues and honour retry-after on 429 responses.
  - At startup and after reconnects, reconcile tracked rooms against the actual
    channels.
- Intents: Guild Voice States, Guild Members (privileged) and Guild Presences
  (privileged; game and stream data).
- Bot permissions: View Channel, Connect, Manage Channels, Move Members, Manage
  Roles and Send Messages. Evaluate effective permissions per channel, because
  category overrides beat role permissions.
- Limits: 100 characters per name, 50 channels per category, user limit 0–99, and
  bitrate capped by the guild's boost tier.
- Batch position updates; never reorder channels on every event.
