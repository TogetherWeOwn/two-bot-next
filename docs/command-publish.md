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
`TWO_VOICE=1` adds the temporary-voice set after moderation (21 new names with
moderation on, because the voice `kick` loses first-wins to moderation `/kick`;
22 with moderation off). `/templateassistant` stays unpublished until its
slice lands with a handler, so `TWO_ASSISTANT_ENDPOINT` currently publishes
nothing. The running bot
publishes the voice set only while its voice sink actually built; this CLI reads
the same env flags and cannot see that, so a diff against a bot whose voice
sink failed to start will show the voice commands as extra desired entries.
Without these gates, only the always-enabled core commands are included. Other
router surfaces have no additional published command definitions here. Invalid
feature configuration fails before a request.

**This is a complete replacement, not a per-command patch.** The CLI refuses
`publish --apply` with exit `1` before PUT if any removal is **unrecognized**:
its raw command type/name is absent from both the desired set and the complete
compiled builtin namespace. The namespace comes from the same core, feature,
moderation and voice definition catalogs as the router, independent of feature
gates; `/templateassistant` is not published and is not included. A user/message
context command sharing a builtin slash name is a different identity. Unknown
response fields cannot turn off this guard. Known disabled builtins retain their
ordinary removal semantics; a custom command squatting a known builtin identity
cannot be distinguished by this name-based guard. Review every removal.

The CLI does not load custom commands from a database or blindly echo remote
definitions. There is **no force/allow-delete flag**. Reconcile unknown/legacy/
dynamic commands through the existing complete registry owner and approved
cutover workflow, which owns the full desired set; do not acquire another token
or use boot publication as a workaround. This CLI guard does not redesign boot
or gateway publication. Do not enable boot publication for a guild that must
preserve a separate dynamic/custom registry. Global application commands and
per-guild user/role permission overrides are not read or modified here.

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
live-guild refusal, `1` omission-guard refusal or REST/read/publication failure.

`--apply` is valid only for `publish`. Without it, no PUT is sent. With it, a
matching hash also sends no PUT. When drift exists, one successful full bulk
replacement is requested at
`/applications/{application}/guilds/{guild}/commands`; transient status retries
use the existing bounded paced executor policy. Failed, forbidden, timed-out,
or malformed reads are never treated as an empty registry and never proceed to
PUT. Duplicate type/name identities and empty names are ambiguous and also fail
closed. A failed PUT does not claim successful publication.

The omission guard inspects the **same fetched snapshot** used to compute the
applied diff, inside the existing sync/read-to-PUT path. There is no disconnected
preflight GET. This is **not Discord compare-and-swap**: an external writer can
change the registry after that read, including during existing transport retry
backoff. Coordinate the complete registry owner during cutover; this change adds
no read/retry/write reconciliation loop or concurrency guarantee.

## Agent JSON mode (schema version 1)

Append the boolean `--json` flag to `diff` or `publish` for exactly one JSON
document on stdout, including errors. Existing human output and exit codes stay
the default, with removal classifications added. JSON dry-run drift still exits
`0` with `ok: true`, even when apply would refuse. No registry request is made for
invalid flags/configuration. Duplicate flags, `diff --apply`, `--json=true`,
`--json` with a value, and combining `--json` with help are usage errors (`2`).

```sh
two-bot commands diff --json
two-bot commands publish --apply --json
```

The fixed contract is:

- `schema_version: 1`, `ok`, and `applied` (only true on confirmed publication).
- `current_hash` and `desired_hash`: canonical 64-character SHA-256 hex digests
  from the read snapshot and desired set. Both are `null` if classification could
  not run; unknown is **not** an empty registry. A failed PUT can retain the
  pre-write hashes; `applied: false` alone is not proof of no remote effect.
- `added_count`, `removed_count`, `changed_count`: complete diff counts, or `null`
  when unavailable. JSON omits raw definitions/field values and remote bodies.
  The human mode still shows nested field changes, bounded to 16,384 characters
  plus a truncation notice; identity/path control characters are escaped and
  terminal escapes and mentions neutralized.
- `removals`: at most 100 records in type/name order, each with `id` (decimal
  string or `null` when Discord omitted metadata), numeric `type`, display `name`,
  `name_truncated`, and `removal_class` (`known_builtin` or `unrecognized`). Names
  are limited to 64 Unicode characters, with controls/mentions neutralized.
  These labels are not input for classification; raw identities and hashes are.
  `removals_truncated` explicitly signals an omitted tail. The JSON document is
  below 64 KiB; **all** removals are classified regardless of reporting limits.
- `has_unrecognized_removals` and a constant, bounded `next_action`. This boolean
  is meaningful only when hashes/counts are available.
- `error: null` on success, otherwise `{code, next_action}`. Stable codes are
  `configuration_error` (exit `2`, no REST), `registry_request_failed` (exit `1`,
  failed/malformed read, admission or publication), and
  `unrecognized_command_removal` (exit `1`, classified refusal, **zero PUT**).
  Error text never echoes arguments, credentials, URLs or remote error bodies.

For an unrecognized removal, `next_action` directs the complete registry
owner/cutover workflow, not a bypass or a production test. Repeated refused
invocations remain read-only. Read-only diff/publish identify the same removals
without treating inspection as permission to apply.

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
an unknown registry. The early sync reads the same env flags as the CLI, so with
`TWO_VOICE=1` it already carries the voice set; keeping it there means a healthy
restart does not delete and recreate the voice commands (which would drop their
command IDs and permission overrides). It runs before the voice sink is built,
so if the sink then fails to build, the voice commands stay published until the
gateway's own registry sync, which runs before shard polling and uses the built
sink's presence, withdraws them. With the opt-in absent, the early sync is
skipped; gateway publication still runs.

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
live-guild fence stays on. `wrangler/scripts/check-env-bindings.py` rejects the opt-in
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

The gateway keeps command identity separately from the drift hash. Successful
bulk PUT receipts replace the application/guild snapshot, including custom-command
republishes; a matching GET that skips PUT also hydrates it. Failed publication
keeps the last successful snapshot. Voice admission and parsing, the shared
router and ordered RSVP preparation share this map: a known slash command ID
selects its registered name, not the payload's name. Canonical names reach
existing ownership and permission checks, preventing competing callbacks while
keeping moderation-first `/kick` and its vote delegate intact.
Unknown or removed IDs cannot claim known voice names. Name fallback is limited
to a registration scope not yet read, or a registration with no ID metadata.
Guild/global scope and command type remain distinct; guild publication does not
hydrate global registrations. Components still use their own custom IDs, and
`/kick` remains owned by the shared router's moderation-first dispatch.

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
python3 scripts/cargo_cache.py run -- test -p two-bot --test commands_registry_removal_guard_acceptance
python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot commands_cli::
```

If pool admission is unavailable, stop local compilation and use hosted CI;
never bypass it with direct Cargo or an alternate target directory. See
[the build-cache runbook](build-cache.md).
