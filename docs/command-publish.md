# Guild command registry drift

`two-bot commands` compares Discord's current guild registry with this build's
feature-gated registry. It uses the existing `InteractionRouter::publish_set`
and Twilight conversion, without changing any command definitions. These
operator commands start no gateway shard. Live Discord targets build shared
send admission from `TWO_DATABASE_URL` (or `DATABASE_URL` when unset), the same
admission Postgres the gateway uses; loopback `DISCORD_API_BASE` fixtures open
no database.

## Configuration and safety

Supply `DISCORD_TOKEN` through the normal secret binding, never a command-line
argument. Set `GUILD_ID` and `DISCORD_APPLICATION_ID` to the guild and application
owned by that bot token, or override the two IDs with `--guild-id ID` and
`--application-id ID`. IDs must be nonzero decimal Discord snowflakes. The existing
`DISCORD_API_BASE` REST proxy seam is honored (normally leave it unset).

Both **diff and publish refuse the live guild** (`326474832151838730`) unless
`--allow-live-guild` is present. The fence runs before REST or database I/O, and
also recognizes numerically equivalent IDs with leading zeros. That flag is an
explicit acknowledgement, not approval for a live rollout: obtain the usual
rollout approval separately.

The desired set is the compiled builtins enabled by the same publishing gates:
`TWO_AUTOMATIONS=1`, `TWO_ANNOUNCEMENTS=1`, `TWO_MODERATION=1` (including its existing
`TWO_OWEN_USER_ID`/protected-role validation), and `TWO_COMMUNITY_SCORECARD=1`.
Without them, only the always-enabled core commands are included. Other router
surfaces have no additional published command definitions here. Invalid feature
configuration fails before a request.

**This is a complete replacement, not a per-command patch.** Remote commands
absent from the desired set are removed, including disabled feature commands,
legacy commands, and dynamic/custom slash commands. These tools deliberately
compare against the *compiled* registry; they do not load custom commands from a
database. Review every removal before applying. Do not enable boot publication
for a guild that must preserve a separate dynamic/custom registry. Global
application commands and per-guild user/role command permission overrides are
not read or modified by this workflow.

## Inspect and publish

With bindings configured for a test guild:

```sh
two-bot commands diff
two-bot commands publish             # dry run, identical comparison
two-bot commands publish --apply     # overwrite only if the hashes differ
```

Explicit ID example (replace the example IDs with the test application/guild):

```sh
two-bot commands diff --application-id 1111 --guild-id 2222
```

Output contains the current and compiled SHA-256 hashes, added/removed/changed
counts, `+` additions, `-` removals, and `~` changed commands. Commands are named
`TYPE/NAME` (`1` = slash command). Changed fields include old/new JSON values and
nested paths such as `options[0].choices[1].value`, `description`, and
`default_member_permissions`. No token is printed. Exit codes: `0` successful
comparison/publication (including drift in a dry run), `2` usage/configuration or
live-guild refusal, `1` REST/read/publication failure.

`--apply` is valid only for `publish`. Without it, no PUT is sent. With it, a
matching hash also sends no PUT. When drift exists, one successful full bulk
replacement is requested at
`/applications/{application}/guilds/{guild}/commands`; transient status retries
use the existing bounded paced executor policy. Failed, forbidden, timed-out,
or malformed reads are never treated as an empty registry and never proceed to
PUT. A failed PUT does not claim successful publication.

## Optional boot publication

Boot publication is new and **off by default**; enabling it does not enable
interaction feature gates. Opt in with:

```text
TWO_COMMANDS_PUBLISH_ON_BOOT=1
DISCORD_APPLICATION_ID=<application owned by DISCORD_TOKEN>
```

For an approved live rollout only, also set:

```text
TWO_COMMANDS_ALLOW_LIVE_GUILD=1
```

In the Worker deployment, configure these as Worker bindings: the container
startup allowlist forwards the publication opt-in, application ID, live-guild
acknowledgement, and the registry feature bindings (including moderation's
protected-user/role settings). None of these are enabled by default. Unrelated
Worker bindings and `DISCORD_API_BASE` are not forwarded into the container.

Boot uses the normal `GUILD_ID`, `DISCORD_TOKEN`, and feature bindings. Normal
gateway prerequisites (including `DATABASE_URL`) still apply. If enabled,
publication/refusal happens before opening the gateway database or connecting
the shard; failure stops the configured gateway task rather than continuing with
an unknown registry. With the opt-in absent, existing server behavior is unchanged.

This synchronizes command definitions only; it does not wire missing interaction
handlers or certify a command cutover. Keep it disabled until the required
handlers are connected and the rollout is approved. A successful replacement
can precede a later database/session/shard startup failure; those failures do
not roll the registry back. Use the explicit CLI dry run to inspect removals
before opting into this boot behavior.

**Staging** opts in from `wrangler.toml` (`[env.staging.vars]`):
`TWO_COMMANDS_PUBLISH_ON_BOOT = "1"` plus the staging bot's public
`DISCORD_APPLICATION_ID`. The container publishes with its own send-admission
database, so an operator CLI run needs no second database credential, and the
live-guild fence stays on. `scripts/check-env-bindings.py` rejects the opt-in
at top level and in production; production publication stays an
Operator-approved Worker binding. The staging guild's registry is a full
replacement on every boot where the hashes differ: do not keep a separate
dynamic registry there. Rollback: delete both lines and redeploy, then publish
the reduced set with the CLI if commands must be removed.

Each enabled boot fetches the full registry, including localizations, and
compares canonical SHA-256 hashes. A matching fetched hash skips PUT. This
fetch-and-compare approach needs no DB publication-hash table, works across
process restarts, and detects out-of-band edits instead of trusting a stale
cached success. Boot logs only the desired hash and whether it applied a write.

Canonicalization ignores server IDs/versions, guild-irrelevant global settings,
command-list order, object-key order, and null/empty/default-false optional
fields. It preserves descriptions, localizations, permission bitfields, command
types, and **option/choice order**. Permission bits unknown to the pinned
Twilight model are retained from the raw response rather than truncated. No
default permission gate is distinct from bitfield `"0"` (administrator-only).
Guild user/role permission overrides are
outside this command-definition comparison.

## Verification

Pure table tests exercise option and choice order, permission bitfields,
metadata/default normalization, and deterministic old/new diff rendering. Mock
REST and binary CLI tests use loopback fixtures; no real Discord guild or
production/staging database is needed.

On the persistent controller, compiling tests must use the bounded Cargo pool:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-discord --lib command_registry
python3 scripts/cargo_cache.py run -- test -p two-bot-discord --test command_registry_sync
python3 scripts/cargo_cache.py run -- test -p two-bot --test commands_cli
```

If pool admission is unavailable, stop local compilation and use hosted CI;
never bypass it with direct Cargo or an alternate target directory. See
[the build-cache runbook](build-cache.md).
