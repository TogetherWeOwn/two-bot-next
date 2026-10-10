# Pre-deploy preflight

`two-bot preflight` answers whether the **supplied deployment configuration** has
working Discord credentials, privileged intents, role hierarchy and channel
access, without connecting to the gateway. It uses Twilight 0.17 request builders
and the shared raw governed transport, issuing **GET requests only**. Live requests
open the runtime `TWO_DATABASE_URL` admission authority and update only the
credential's durable admission lane. It does not query configuration tables, open
Redis, migrate, send messages, grant roles, or repair permissions. A held or
unavailable lane stops checks; 429 is recorded by the shared gate and never
retried by this command. See [durable send admission](discord-send-admission.md).
Running it against the production guild is an operator step in the cutover
runbook, not an agent test or part of this implementation's verification.

## Usage

Load the deployment's environment through its normal secret/configuration
mechanism (do not put a token on the command line):

```sh
two-bot preflight --help
two-bot preflight
two-bot preflight --json
```

Required environment:

- `DISCORD_TOKEN`: the runtime bot credential. `DISCORD_BOT_TOKEN` is accepted
  only when `DISCORD_TOKEN` is absent (unset), for legacy operator environments.
  A present-but-empty primary is a configuration error, not permission to try
  the alias. A rejected credential stops the check; there is no retry with the
  alias.
- `TWO_DATABASE_URL`: the same durable admission authority used by the runtime,
  with migration 0361 already installed. It is not an alternative data target;
  no live request is permitted without this authority.
- `GUILD_ID`: one nonzero guild snowflake, pinned to the deployment under test.
  `DISCORD_GUILD_ID` is a legacy fallback when `GUILD_ID` is absent/empty.
- The same feature/channel configuration supplied to the bot at startup.

The token, response bodies, server-side names and malformed configuration values
are never printed. Errors show static guidance or an HTTP status code, not a
Twilight debug/error dump. No secret is a CLI argument.

### Database-backed level rewards

Level rewards are configured in `level_role_rewards`, not an environment list.
To avoid reading configuration tables, the operator supplies the guild's
**current exported role IDs**:

```sh
two-bot preflight --level-role-ids '111111111111111111,222222222222222222'
two-bot preflight --level-role-ids '' # explicit assertion: no configured rewards
```

The export must correspond to the target guild's
`SELECT role_id FROM level_role_rewards WHERE guild_id = <target> ORDER BY level`.
Use the approved read/export workflow; this command neither runs that query nor
verifies export freshness. Do not supply a guessed/partial list. A missing export
is **FAIL** when `TWO_ONBOARDING_MODE` permits level-role writes (`legacy`, the
unset default, or `anchor`) and `TWO_ONBOARDING_DRY_RUN` is not `1`. In `session`
mode or onboarding dry-run, level/game role writes are disabled, so no reward
export is required. The report names this coverage gate explicitly.

## Checks

1. `GET /users/@me`, then `GET /applications/@me`: proves the token is accepted
   and the application can be read. Any REST error stops further checks; no
   alternate credential is tried.
2. Application approved **or limited** flags for Guild Members, Message
   Content and Presence are compared to `gateway::intents_from_env()`. Members
   are always requested. Content is requested for `TWO_AUTOMOD=1` **or** all
   three nonempty ticket settings (category, staff role, panel channel).
   Presence is requested for `TWO_VOICE=1` with `TWO_VOICE_PRESENCE=1`
   (`docs/voice-presence.md`). Missing a requested portal flag is FAIL; an
   unused enabled flag is WARN.
3. The bot's guild member and all guild roles are fetched. Permissions are the
   union of `@everyone` and the bot's held roles. The legacy funnel/internal-action
   grant is checked: Manage Server, View Channels, Create Instant Invite, Manage
   Roles, Manage Events and Send Messages. Administrator satisfies these bits but
   is always WARN because it bypasses channel overwrites. Invite-list readability
   is also checked with a real GET.
4. Every supplied level reward and every `TWO_SELF_ROLE_PANELS[].options[].roleId`
   must exist, be unmanaged, not be `@everyone`, and be strictly below the bot's
   highest role. Equal-position roles use Twilight's Discord snowflake ordering.
   Administrator **does not bypass hierarchy**. The self-role catalogue is
   parsed with the same strict parser as gateway boot
   (`parse_self_role_panels`), so Discord bounds (20 reactions, 100-unit
   button custom ids, 80-unit button labels) fail preflight before any REST.
   Live role/channel safety beyond the catalogue still validates at runtime.
5. For production's built-in onboarding catalogue, game **and platform** role IDs
   and primary/fallback channel IDs are checked when the game picker is enabled.
   Production catalogue IDs are never applied to another guild, and `session` or
   onboarding dry-run does not require these role-write targets.
6. Each configured channel is fetched individually, must belong to the target
   guild, and is resolved from its **own** overwrites. A category allow is not
   assumed to grant access to an unsynced child. Posting destinations must be text
   or announcement channels with View, Send and Embed. ManageMessages is required
   for text channels covered by enabled automod, except its exemption list. All
   four effective booleans are printed for every channel. A denial/missing channel
   is FAIL, not a skipped PASS.

### Channel target census

Posting (View/Send/Embed):

- `DISCORD_ANCHOR_WELCOME_CHANNEL_ID`
- `DISCORD_AUDIT_LOG_CHANNEL_ID`
- `DISCORD_GOODBYE_CHANNEL_IDS`
- `DISCORD_LANDING_CHANNEL_IDS`
- `DISCORD_MODERATION_LOG_CHANNEL_ID`
- `DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID`
- `DISCORD_STAFF_ALERT_CHANNEL_ID`
- `DISCORD_TICKET_PANEL_CHANNEL_ID`
- `DISCORD_VOICE_LOG_CHANNEL_ID`
- `TWO_TEMP_VOICE_PANEL_CHANNEL_ID`
- Self-role panel `channelId`

View-only references (voice rooms/categories, protected/exempt lists and human
community destinations):

- `DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID`
- `DISCORD_TICKET_CATEGORY_ID`
- `TWO_TEMP_VOICE_GENERATOR_CHANNEL_ID`
- `TWO_TEMP_VOICE_CATEGORY_ID`
- `TWO_TEMP_VOICE_PROTECTED_CHANNEL_IDS`
- `TWO_AUTOMOD_EXEMPT_CHANNEL_IDS`
- `TWO_COMMUNITY_HUMAN_CHANNEL_IDS`
- `TWO_COMMUNITY_WELCOME_CHANNEL_IDS`
- Applicable onboarding game primary/fallback destinations

Lists are comma-separated nonzero snowflakes. Repeated channel IDs are fetched
once, combining their requirements. No configured channels produces a coverage
WARN, not an assertion that destinations work. Settings stored only in
`guild_settings` must first be supplied as their runtime environment values.
Dynamic/database-only destinations (scheduled messages, feeds, ticket instances,
etc.) and arbitrary game mappings on other guilds are not discovered by this
command. A PASS is not proof of complete database inventory, moderation target
hierarchy, voice connectivity, thread membership, or end-to-end feature delivery.

## Permission resolution

The pure `two_bot_discord::channel_access` resolver follows Discord's documented
order: guild role union, Administrator short-circuit, `@everyone` overwrite,
unioned held-role denies then allows, member-specific deny then allow. View denial
makes Send, Embed and ManageMessages ineffective; Send denial makes Embed
ineffective. Role positions do not affect overwrite precedence.

Sources:

- [Discord permission overwrites and implicit permissions](https://docs.discord.com/developers/topics/permissions#permission-overwrites)
- [Discord permission syncing (not category inheritance)](https://docs.discord.com/developers/topics/permissions#permission-syncing)
- [Discord role hierarchy](https://docs.discord.com/developers/topics/permissions#permission-hierarchy)
- [Application flags](https://docs.discord.com/developers/resources/application#application-object-application-flags)
- Legacy `scripts/preflight.ts`, `src/discord/channelAccess.ts`,
  `test/unit.channelaccess.test.ts`, `test/unit.gamechannelaccess.test.ts` in
  `TogetherWeOwn/two-bot` (source read at `96777468472f23a02a1e97a43ffab3912fe5df2a`).

## Output and exit codes

| Code | Meaning | Deploy interpretation |
| --- | --- | --- |
| 0 | PASS/WARN only | Checked configuration works; inspect warnings before deploy |
| 1 | At least one FAIL | Stop; requested intent/access/hierarchy or coverage is broken |
| 2 | Missing credential/guild, malformed config, invalid argument/test seam | Fix invocation/configuration; no Discord checks performed |

The default output is a human-readable `STATUS / CHECK / DETAIL` table. `--json`
prints a single object with `schema_version: 1`, `ready`, `exit_code`, `failures`,
`warnings` and `checks[]` (`status`, `check`, `detail`). Help exits 0 without
credentials or REST. WARN never changes exit 0 into failure. The command does not
fix anything it finds: remediation/deploy authorization remains a separate step.

## Offline verification

```sh
cargo test -p two-bot-discord --locked channel_access --lib
cargo test -p two-bot --locked --test preflight
```

Acceptance launches the **real binary** against the existing scripted mock REST
double, with a cleared environment and fake token. It covers PASS/WARN/FAIL,
intent gates, role hierarchy/managed/deleted roles, channel denials, JSON/table
output, credential rejection, invalid config and zero API writes. The fake REST
names and error bodies intentionally contain the fake token to detect accidental
logging. With only the explicit loopback fixture and no `TWO_DATABASE_URL`, no
admission database is needed. A present but failed authority is never replaced
or bypassed. Negative coverage proves live requests without the authority are
refused before HTTP, and preflight 429 cannot trigger a hidden resend.

`DISCORD_PREFLIGHT_API_BASE=http://127.0.0.1:<port>` (or another literal loopback IP)
is the test-only HTTP seam. It rejects non-loopback hosts, URL paths, credentials
and query strings before any request, so a token cannot be redirected to an
arbitrary endpoint. Do not set this variable in a real deployment.
