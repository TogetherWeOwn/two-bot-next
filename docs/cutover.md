# Production cutover and rollback (B4)

Execution card: [TOG-9699](/TOG/issues/TOG-9699). This runbook is a
**procedure, not approval to execute it**. It replaces the legacy Coolify bot
with the Rust Cloudflare Container, using the **same Discord application**.
Exactly one gateway owner and one set of job/effect writers may run at a time.
Keep the legacy image, configuration and database recovery points warm for the
whole 48-hour watch. Do not remove Coolify resources at watch close.

## Roles, safety and evidence

- **Cutover lead:** DevOps & Reliability Engineer on B4; owns go/no-go, timing,
  watch and rollback. Director of Engineering resolves technical decisions.
- **Data lead:** shared-database migration owner; owns copy, verification and
  reverse reconciliation. Moderation lead accounts for outstanding sanctions.
- **Executor:** authorized deployment/data broker; host-only steps use the
  existing `Operator:` handoff, one per step, with command and rollback.
- **Reviewer:** independent exact-SHA approval and green CI are prerequisites,
  not something the deployer may waive. Missing evidence means **NO-GO**.

Record UTC times and non-secret receipts on B4. Store member data, dumps,
command payloads and logs in the restricted backup/evidence location, not in
public GitHub or issue comments. Commands below are either local inspection or
instructions for a **separately authorized executor**. Never run tests, probes
or verification queries against production/staging databases from an agent
workspace: rehearse with fixtures or `agent-testdb` / `agent-testredis` / CI
service containers. Production copy/verification belongs to the authorized
data-migration step, not an ad hoc test. If a credential fails, stop and report;
never substitute another credential. Never rotate/delete tokens as a workaround.

Create a B4 evidence manifest before scheduling:

| Receipt | Required content (names/IDs only for secrets) |
|---|---|
| Targets | Legacy Coolify UUID, Container/Worker environment and internal URLs, Discord application/guild IDs; verify current inventory, do not reuse historical IDs blindly |
| Releases | Legacy image digest + commit, Next digest + reviewed head + merged commit, Worker version/config, required checks |
| Data | Current canonical database, every writer (bot, web, queues, cron), table/key ownership, migration versions, snapshot IDs/hashes, generated-key allocator/high-water receipts, restore receipt |
| Registry | Global/guild command-definition and separate guild permission snapshots at pre-swap and rollback freeze; defaults/inheritance, approved watch edits, old/current/restored ID maps, reconciled target and read-back receipts |
| Ownership fence | Reviewed Worker/DO maintenance mechanism and version, singleton identity, persisted fence receipt, scheduled/startup-path rehearsal and explicit release owner |
| Secret bindings | Existing Discord token binding, database binding, internal-action signing binding and intended consumers; never values |
| Gates | Parity report, soak sign-off, W16 all-warm rollback drill, region/access sign-off, moderator sign-off |
| Window | Freeze time `T_f`, Next first ready `T_0`, rollback freeze `T_r` if any, watch deadline `T_0 + 48 h`, named on-call coverage |
| Recovery | Approved reconciliation mapping, journal/change-capture location, latest durable watermark, tested restore time and maximum loss window |

## Preconditions: all must pass

- [ ] `docs/parity.md` has **zero unmapped** legacy behaviors. Mapping is not
  implementation: every non-DROP behavior has merged runtime wiring and passing
  acceptance evidence, or an explicitly approved waiver. Moderation, automation,
  scheduled unbans and internal actions cannot be silently waived.
- [ ] [B2 soak](staging-soak.md) is signed off: four fully evidenced ACTIVE
  hours with zero missed events across joins, voice, messages and slash; every
  actual outage recovers in under 60 seconds from outage start to verified
  recovery; hours 3–4 include 120 expected one-minute samples. Flat memory and
  no error spikes remain required (numeric RSS/error criteria are unaccepted).
  Include feature/voice evidence, not just health polls; deploy-finish-to-ready
  is not outage-recovery evidence. The approved policy does not operationally
  define ACTIVE hours, the required samples' source/shape, or the hours 1–2
  sample rule; no verified live route currently proves the four-family evidence
  or 120 samples, so B2 remains NOT VERIFIED and cannot be signed off on the
  current documentation. The documented staging redeploy interruption is
  95–139 seconds (with a separate historical 92–139-second staging note); if
  the approved policy counts planned redeploys as actual outages, these exceed
  the 60-second limit. That classification is unresolved and no owner is named
  in the approved text; do not infer a PASS.
- [ ] W16 all-warm rollback rehearsal has passed and shared Neon data/region,
  schema compatibility and backups are signed off. B4 runs **last**. The old
  migration plan's TypeScript/no-copy assumptions are not evidence for the Rust
  schema: resolve actual table compatibility before moving data.
- [ ] Candidate is merged, independently reviewed on its exact head, and
  `ci-ok` (the full verdict over lint, worker checks and all selected Rust/DB
  test lanes), `worker check`, `pr-lint` and `gitleaks` are green on that head.
  A green lint-only `check` is not enough. Pin the resulting deployment digest.
- [ ] Each tool required below is merged into that candidate; its argument
  parsing, safety behavior and rollback have been rehearsed with disposable
  fixtures. Inspect source before invoking any help flag: in this baseline,
  **only top-level `two-bot --help` is approved help-only inspection** (see below).
  A planned subcommand is **not executable evidence**.
- [ ] A reviewed, rehearsed **Worker/DO ownership fence** blocks every Next
  Container auto-start/reconnect path, persists across restarts/deployments and
  stays active throughout legacy ownership. A stopped Container, disabled
  external monitor or blocked public route alone is insufficient. The baseline
  has no such fence; missing implementation/rehearsal is **NO-GO**.
- [ ] Restore drill proves both database recovery and replay suppression, not
  just that a dump can be read. Data written by Next **and the web** during the
  watch can be read by legacy or reverse-reconciled without silent loss.
- [ ] Bot application identity is unchanged; web OAuth/internal-action callers
  use that application and the agreed endpoint/signing contract. Secret
  provisioning is by an authorized principal, under existing grants; new
  credentials/rotation require the applicable reserved gate.
- [ ] Command restoration is rehearsed for **both global and guild scopes**,
  including the separate permissions procedure below. Verify existing authorized
  OAuth2 Bearer access for permission restoration before any rename/removal;
  the bot token is insufficient for that write. Missing access/mapping is NO-GO,
  not permission to obtain/substitute another credential. Full replacement
  deletes omitted slash/user/message commands; no unrelated command is dropped.
  Rehearse rollback reconciliation of watch-window registry/permission changes,
  including a revoked role allow, changed inherited defaults and command
  additions/deletions/renames. Restoring the frozen baseline must not undo those
  changes or broaden current access; unsupported drift keeps commands frozen.
- [ ] A durable journal/watermark covers every affected table, deletes and
  side-effect ledger, or a tested shared-DB compatibility path preserves them.
  **No complete rollback data path = NO-GO**, even if gateway health is green.

## T-minus checklist

**T−24 h:** attach the manifest and sign-offs to B4; announce the maintenance
window to moderators. Pin both artifacts. Verify backup retention exceeds the
watch/reconciliation period, restore access is available, and no deployment or
schema migration will race the window. Define a maximum maintenance gap and
rollback completion budget using the drill; a missed budget is an incident, not
permission to skip reconciliation. Arrange manual moderation coverage while the
bot is disconnected.

**T−60 min:** record outstanding tempbans/unbans, timeouts, tickets, voice rooms,
feeds, scheduled messages, pending web/internal actions and retry/delivery
leases. Moderators identify sanctions falling due during the freeze. Preserve
absolute expiry times, IDs and completed/claimed states, not just row counts.
Check role hierarchy/channel permissions, privileged intents, dry-run versus
enforcement policy and audit destination. Confirm all job/cron/queue consumers
that can mutate the copied data can be paused and resumed without duplication.

**T−15 min:** run read-only command diff and preflight with the production
application/guild through the authorized REST executor (no gateway startup).
Capture token-valid/application identity, intent flags, role/channel results and
the `session start budget` check (`GET /gateway/bot`: `remaining`, `total`,
`reset_after_ms`, `max_concurrency`); FAIL is NO-GO (exit 1), including
`remaining` below 10 or an unreadable budget, while WARN (`remaining` below
100) needs a recorded disposition but keeps exit 0. Snapshot
legacy configuration, command definitions **and separate guild permissions**
*before* any overwrite. Confirm the legacy restart path does not auto-register a
different registry or resume a stale session. Verify the persisted Worker/DO
fence already prevents Next startup while legacy owns the application, including
health callers and already scheduled keepalive work. Reconfirm first Next boot
has RESUME disabled (see below).

## Freeze and drain (T_f)

1. Announce the start, freeze new bot commands and web/internal-action writes,
   and pause all identified producers/cron and consumers. Account for web
   writes even when the web already uses Neon. Pause only the affected features
   via the reviewed maintenance mechanism; do not guess environment flags.
2. Let admitted handlers and Discord requests finish. Record queue depth,
   in-flight requests, due unbans, last successful jobs and delivery/replay
   ledgers. Drain must reach zero admitted work, or explicitly preserve and
   reconcile each pending item. A process stop alone is not proof of drainage.
3. Gracefully stop legacy via Coolify/broker, recording its last event and
   terminal process/container state. Confirm auto-deploy, restart supervision
   and duplicate replicas cannot reconnect it. Keep its pinned image/config
   warm but **stopped**, not a second gateway. Confirm no remaining DB/effect
   writer; if graceful shutdown times out, treat effects as uncertain and
   reconcile them before continuing.
4. Pending unbans retain their deadlines across the freeze. Moderation lead
   handles any overdue action through the authorized route and records whether
   Discord actually applied it. Import pending/completed state once; never
   issue an extra ban, unban, kick, timeout or warning merely to test parity.
5. Record `T_f` and final writer watermarks. If any writer cannot be fenced,
   **abort before data copy/registry mutation** and restore legacy ownership.

## Tool availability and command sheet

Baseline checked: `44338b2` on 2026-09-30. Recheck candidate **source parsing**
before invoking tools. `two-bot` with no subcommand starts the server;
`two-bot serve` is **not** supported. For baseline help-only inspection, use
**only `two-bot --help`** (top-level).

Do **not** run `two-bot backup --help` or
`two-bot guild-config-snapshot --help`: dispatch ignores their trailing arguments
and executes the backup/prune/upload or Discord snapshot/upload instead. The
usage comment claiming help after any subcommand is not the dispatch behavior:
[argument dispatch](../crates/bot/src/backup_cli.rs#L162). Other binaries or future
subcommand help paths require source/fixture verification before approval; a
`--help` suffix is not a read-only safety boundary.

### Existing tools: limited recovery, not the full cutover data path

| Implemented command | Binding / important limit |
|---|---|
| `two-bot backup` | `TWO_DATABASE_URL`, `TWO_BACKUP_DIR`, `TWO_BACKUP_KEEP`, `TWO_BACKUP_UPLOAD_CMD`; consistent allowlisted snapshot, then retention/pruning and upload. Never point retention at incident-preservation evidence |
| `two-bot restore <backup.ndjson.gz> --force [--dry-run]` | `TWO_RESTORE_URL` deliberately differs from the source binding. Actual restore **truncates/replaces** allowlisted tables; it is not a merge/delta reconciliation. Target schema must already exist |
| `two-bot backup-upload <dump.ndjson.gz>` | Uploads one dump using approved S3 bindings; an upload is not proof of completeness or restore compatibility |
| `two-bot guild-config-snapshot` / `two-bot guild-config-restore --snapshot FILE` | Pinned **staging-only** guild structure recovery. Does **not** snapshot application commands; never use as production registry rollback |

Reference: [backup CLI commands/bindings](../crates/bot/src/backup_cli.rs#L99),
[restore implementation](../crates/bot/src/backup_cli.rs#L380) and
[backup runbook](backup.md). `restore --dry-run` optionally reads a target when
`TWO_RESTORE_URL` is set; do not mistake it for automatically offline operation.
For local fixture/artifact inspection **without any DB connection**:

```sh
# File-integrity receipt only; does not establish table/content parity.
sha256sum --check receipts.sha256
# No target URL: validates the NDJSON/gzip manifest and reports its contents.
env -u TWO_RESTORE_URL two-bot restore fixture.ndjson.gz --dry-run
```

Use a disposable fixture for a test. Missing/tampered file or nonzero exit means
FAIL; on a real restore require exit 0 and `RESTORE VERIFIED`, then separately
verify canonical content and required table coverage. A v4 dump covers every
table that either migration chain creates, except `xp_cooldowns`, the store
chain's `rollback_journal`/`rollback_watermarks` and the migration ledgers
([`EXCLUDED_TABLES`](../crates/core/src/backup/dump_file.rs#L141)),
plus retired legacy tables when the source still has them
([`DUMP_TABLES`](../crates/core/src/backup/dump_file.rs#L44)). Restore needs a
target migrated to the dump's schema, and refuses before any write when a
current table is missing. Coverage and the trigger and sequence handling are in
[backup.md](backup.md#coverage-and-recovery-semantics-v4-tog-11142). Do not
treat its per-table count checks as complete final-copy verification.

MEE6 XP/backfill/capture/reward utilities are **separate binaries**, not a
legacy-table copier or rollback journal. Some default to writes, and even some
previews run migrations on connect; they are not production preflight tools.
Do not use `dedupe-events` during cutover: it defaults to deletion and has no
guild fence. Their exact behavior is in the
[cutover CLI sources](../crates/cutover/src/bin) and is outside this procedure.

### Planned copy, registry and preflight tools

The following are **planned, not merged into this baseline**; do not paste them
into a production shell or invent flags to make them work:

| Planned invocation | Owner / use / missing evidence |
|---|---|
| `legacy_copy` (separate cutover binary), default dry-run; `--apply`, `--allow-live-target` planned | [TOG-10868](/TOG/issues/TOG-10868): per-group legacy→Next mapping and row-count plan. Pending schema groups must fail, not be omitted. Exact source/target binding flags and verification invocation require merged tool documentation. A forward upsert is **not** a rollback delta exporter |
| `two-bot commands diff` | [TOG-10860](/TOG/issues/TOG-10860): read current guild registry and compare to compiled desired registry; no PUT/no gateway |
| `two-bot commands publish --apply` | Same card: explicit overwrite; default dry-run and live-guild opt-in required. Verify final flag spelling after merge; global snapshot/restore is not implied by a guild tool |
| `two-bot preflight --json` | [TOG-10858](/TOG/issues/TOG-10858): read-only REST identity/intents/role/channel checks, FAIL vs WARN. Verify target selection and live-target guards after merge |

No `commands restore`, complete database delta export/reverse-import, or
`--disable-resume` command is established by this baseline. Gateway RESUME is
automatic; there is no verified disable environment flag either. Do not invent
`RESUME=0`, delete session rows or omit the database to force a fresh session.
These are required **capabilities**, not claimed existing subcommands. B4 must
attach a reviewed, fixture-rehearsed execution/restore command sheet covering
them before GO. Likewise, the merged generic backup is not a complete snapshot
of all Next state; a backup upload receipt alone cannot satisfy the data gate.

### Worker/DO ownership fence: implemented; rehearsal still required

The persisted fence is implemented in [`ownership.ts`](../wrangler/src/ownership.ts)
and the [`Worker/DO wrapper`](../wrangler/src/index.ts), using pinned
`@cloudflare/containers` **0.3.7**. Implementation is not a staging receipt or
production authorization. **No rehearsed durable fence = NO-GO** for B4.

The singleton remains `TWO_BOT.getByName("two-bot")`. Its SQLite DO storage owns
`two-bot:owner:v1`: `deploymentId`, monotonically increasing `epoch`, `phase`,
`actor`, `timestamp`, `oldEpoch` and `oldDeploymentId`. `deploymentId` is the
Cloudflare **Worker version ID** from `CF_VERSION_METADATA.id` (not a Git SHA,
release label, or Cloudflare deployment resource ID). The Worker overwrites the
ingress identity header, and the DO requires caller identity, its own version,
and the active stored owner to agree. Missing metadata/record, malformed state,
a storage read failure, another version, or `phase=fenced` refuses with 503
`ownership_fenced`, without forwarding/startup. There is no first-request claim.

All supported SDK startup entries (`start`, `startAndWaitForPorts`,
`containerFetch`) and health/readyz, keepalive and `onStart` scheduling are gated.
A DO concurrency gate drains admitted starts/probes before ownership changes;
legacy/stale keepalive payloads cannot renew, probe, or rearm. Constructor
reconciliation destroys an inactive already-running process. The base SDK alarm
is left intact; only application `keepalive` schedules are removed. Do not
clear DO storage or SDK alarms. The fence is **per singleton/namespace**: it
cannot revoke legacy, another namespace, or a separate directly started gateway.
Those writers still need their own containment receipts.

#### Authenticated control contract

`GET /internal/ownership` reads the current version, owner record (or `null`) and
native `running` flag without starting the Container. `POST` takes JSON:

```json
{"action":"takeover","expectedEpoch":0,"actor":"release-operator"}
```

Both require `Authorization: Bearer <OWNERSHIP_CONTROL_TOKEN>` at Worker and DO;
the dedicated secret must already be approved/provisioned in that environment.
Absent/short/mismatched tokens return 401. It is never forwarded into the bot.
The actor is an authenticated caller's audit label, **not** independent identity
proof. Bodies are bounded to 1 KiB. Takeover targets only the currently executing
version; there is no client-supplied target deployment. `expectedEpoch` is 0 only
for a never-initialized record; a stale/replayed epoch returns 409. It is not safe
to invent 0 or retry a conflict without reading and reconciling the new state.

Each accepted change increments the epoch, atomically persists a **fenced**
owner and audit receipt, then awaits native destruction and checks `running=false`.
Only a successful takeover writes `phase=active`; it does **not** start the new
container. A stop/crash/final-write failure leaves persisted denial and must be
reconciled with a fresh authenticated epoch change. The independent audit keys
`two-bot:ownership-audit:v1:<epoch>:fenced|active` retain actor, timestamp, old/new
epoch and owner identities. Logs emit only the receipt/fixed refusal reason.
`{"action":"fence","expectedEpoch":N,"actor":"release-operator"}` leaves
`deploymentId=null`, blocking **all** versions until explicit takeover. A
fence failure does not prove the old gateway has stopped: require control-plane
termination evidence before starting legacy.

Use the environment-bound client in the [runbook](runbook.md#persisted-ownership-control)
for staging. Production execution stays on B4's reviewed authorized sheet. The
staging workflow validates the control configuration **before deploy**, then
transfers only an active owner. An uninitialized/parked singleton requires the
explicit `workflow_dispatch.release_fence=true` handoff; a push cannot release it.

#### Required staging rehearsal receipts

1. Verify staging Worker, DO namespace/singleton, staging guild/token binding and
   staging database binding. Retain exact Git SHA, versions A/B and image. Never
   copy a production binding. Provisioning/rotation of the control credential is
   a separate governed step, not authorized by these commands.
2. Read authenticated state; take over A using its exact current epoch. Check
   actor/time/old-new epoch receipt and `running=false`. Probe health/readyz to
   start A, verify one gateway session and retain its keepalive epoch.
3. Deploy fence-capable version B against the **same** namespace/singleton,
   without takeover. Before health traffic, confirm constructor reconciliation
   stops the old process. A stale A ingress and B health/readyz must be refused;
   neither may start/forward. Retain storage readback and container/log receipts.
4. Authenticated takeover of B must increment the epoch and confirm teardown
   before release. Replaying the previous epoch must return 409. A/B parallel
   probes and old A/epoch keepalive work must not produce a second owner. Start
   B only by a subsequent allowed probe; record readiness/gateway evidence.
5. Apply `fence` (parking owner), verify `running=false`, pause external health
   callers and observe beyond **two configured keepalive intervals**. Probe
   both health routes: require 503 ownership refusal, no forwards/starts and no
   new gateway session. Reconstruct/evict the DO and deploy/roll back between
   **fence-capable** versions; the parked record/epoch must survive. Local
   Miniflare fixtures prove read-error behavior and reload persistence; they do
   not replace these staging termination/session receipts. Never fault-inject
   storage by clearing or corrupting the deployed DO.
6. Keep the fence active throughout legacy ownership, recovered monitors,
   deployments and retirement wait. Only release Next after other writers,
   data/registry gates and the lead's handoff are confirmed. Restoring an active
   version without a new takeover is not a rollback release procedure.

**Do not roll back to a pre-fence wrapper:** that code ignores persisted state.
Retain a reviewed fence-capable known-good Worker/image pair before rollout.
Merely stopping a process, pausing a monitor or changing routing never replaces
this control operation. If durable containment cannot be established, preserve
maintenance and escalate to the Director of Engineering; never start legacy
alongside an unconfirmed Next gateway.

### Runtime gates found in the baseline

- Server binds `DISCORD_TOKEN`, `DATABASE_URL`, `GUILD_ID`, and optional
  `LISTEN_ADDR`. Cutover utilities instead use `TWO_DATABASE_URL`, and some
  use `DISCORD_GUILD_ID`. Do not silently substitute bindings. Worker forwarding
  currently covers token/database/guild/listener, **not feature flags**:
  [configuration](../crates/core/src/config.rs#L34),
  [tool connection](../crates/cutover/src/cli.rs#L102),
  [Worker environment](../wrangler/src/index.ts#L73).
- Configured server startup runs embedded migrations; it is not a read-only
  preflight. Gateway boot wires the funnel pipeline, not the full feature/job
  stack. Domain/store libraries and command definitions are not activated
  runtime evidence: [boot](../crates/bot/src/main.rs#L78),
  [pipeline construction](../crates/bot/src/gateway.rs#L336).
- Fresh checkpoints (up to 15 minutes old) automatically trigger RESUME.
  Persistence deduplicates funnel batches, not all Discord effects:
  [session policy](../crates/core/src/gateway_session.rs#L4),
  [boot resume](../crates/bot/src/gateway.rs#L315). A reviewed force-fresh path is
  required for first production boot; do not assume a restart gives IDENTIFY.
- SIGTERM drains accepted gateway dispatches, jobs and HTTP within
  `SHUTDOWN_TIMEOUT_SECONDS` (default 35 s), then exits; a second signal exits
  immediately. See [Shutdown](configuration.md#shutdown).
- The baseline has no wired scheduled-unban handoff/sweeper. Moderator sign-off
  must identify a verified executor for every pending deadline before GO:
  [moderation port boundary](../crates/core/src/moderation.rs#L7).
- `/health` reports process liveness; `/readyz` covers process + gateway only.
  The baseline server does not expose `/internal/actions` or `/metrics`:
  [HTTP routes and readiness](../crates/bot/src/server.rs#L21). Feature and
  internal-action acceptance therefore require separate merged runtime evidence.

These are execution blockers to resolve on the implementation/acceptance cards,
not features delivered by this documentation PR. Do not bypass them with a raw
REST PUT, a forced dump restore or a manual database edit.

### Command definitions and separate guild permission recovery

For **command definitions**, the authorized REST tool must cover:

```text
GET /applications/{application.id}/commands?with_localizations=true
GET /applications/{application.id}/guilds/{guild.id}/commands?with_localizations=true
PUT /applications/{application.id}/commands
PUT /applications/{application.id}/guilds/{guild.id}/commands
```

Keep raw GET snapshots and separately validated PUT payloads; response-only
fields require filtering. Restore every affected scope, including user/message
commands, not only slash commands. A definition GET/PUT does **not** snapshot or
restore guild role/user/channel overrides. Discord warns that **deleting or
renaming a command permanently deletes its permissions**. Recreating a name
alone cannot recover them.

Before any rename/removal/overwrite, capture **separate permission snapshots**
for every affected guild, including guild overrides on global commands:

```text
GET /applications/{application.id}/guilds/{guild.id}/commands/permissions
GET /applications/{application.id}/guilds/{guild.id}/commands/{command.id}/permissions
PUT /applications/{application.id}/guilds/{guild.id}/commands/{command.id}/permissions
```

The first GET captures all returned permission objects, including application
ID defaults for commands without explicit overrides; the second supports
per-command read-back. Retain role/user/channel IDs, types and allow/deny values,
including `guild_id` (`@everyone`) and `guild_id - 1` (All Channels), plus whether
a command is synced to defaults or has explicit overrides. Capture definitions
and permissions as one frozen registry baseline; reconcile any concurrent admin
change before proceeding. Database journals do **not** capture registry or
permission edits made directly in Discord.

Recovery order through the authorized permission executor:

1. **Before any definition PUT**, freeze all registry/permission writers,
   including administrator edits and automatic command sync. At rollback freeze,
   capture final live definitions in every global/guild scope and separate guild
   permission objects, application defaults and synced/unsynced state. Preserve
   this restricted snapshot even for commands about to be renamed/deleted. If
   writers cannot be fenced or the capture is inconsistent, keep commands frozen
   and do not overwrite the registry.
2. Approve a **reconciled target**, not an automatic reset to `T_f`. For initial
   cutover this is the reviewed Next registry with preserved access controls;
   for rollback compare the pre-swap baseline, post-swap receipts, final live
   snapshot and approved watch edits. Map old/current command identities to the
   legacy-compatible target by application, scope, guild (where applicable),
   type and approved name/rename lineage. Preserve legitimate additions,
   deletions and renames; an unsupported definition or ambiguous map is NO-GO,
   not permission to discard it or recreate a deliberately deleted command.
   Carry current restrictions and revocations into defaults and explicit
   overrides. Removing an allow entry is a revocation too: do not reintroduce it
   from the baseline. Compare effective access, including definition defaults,
   role/user/channel precedence and inheritance; a naive union of permission
   arrays is not reconciliation. Do not silently broaden current access. Any
   unexplained drift needs a named moderator/data-lead disposition; unresolved
   compatibility/access changes keep commands frozen and go to the Director of
   Engineering. Retain the approved target and mappings before mutation.
3. Apply reconciled command definitions first; retain each API response's
   **actual** IDs and complete old/current/restored ID maps. Do not assume
   recreated commands reuse old IDs or match only by name. Unmapped/ambiguous
   IDs are NO-GO. Reapply each reconciled explicit override set to the mapped
   command's per-command PUT route with `{"permissions": [{"id": "<resource-id>",
   "type": 1, "permission": false}]}` (illustrative shape only). PUT replaces
   that command's overrides; use the complete approved reconciled array, not a
   partial patch or stale saved array. Do not convert inherited defaults into
   explicit per-command overrides. The batch `PUT .../commands/permissions`
   endpoint is **disabled** and is not a fallback.
4. Verify application-level defaults and synced/unsynced behavior separately
   against the reconciled target. The docs describe application-ID objects on
   GET but do not establish an application-ID PUT shortcut here. B4 must provide
   a separately authorized, rehearsed preservation/restoration path for those
   defaults; do not invent a command ID or endpoint. An unresolved default
   mismatch blocks reopening.
5. Read back guild-wide and mapped per-command permissions and compare all
   resource/type/allow tuples, default inheritance, effective access and command
   definitions to the **approved reconciled target**, not merely the frozen
   pre-cutover baseline. Record mappings and zero unexplained mismatches before
   either cutover GO or reopening legacy commands. Keep registry/permission
   writers fenced through read-back; any concurrent change requires recapture
   and reconciliation before reopening.

Permission **writes require an existing authorized OAuth2 Bearer token** with
`applications.commands.permissions.update`, not the bot token used for command
definitions. The authorizing user must have Manage Guild and Manage Roles,
permission to run the edited command, and permission to manage the affected
resources. Verify that authorized route in the rehearsal; never request/export
credentials in arguments, reuse an unrelated credential or treat an access
error as permission to substitute tokens. Missing scope/user authority/tooling
means NO-GO and manager/CISO provisioning review, not an ad hoc bot-token PUT.

Source: [official Discord permissions and API reference](https://docs.discord.com/developers/interactions/application-commands#permissions),
including permission objects, per-command overwrite, disabled batch update and
rename/delete warning. These routes describe requirements for the separately
authorized tool; they are not executable commands or a credential grant.


## Data copy and verification

Choose and record **one** path with the data lead:

- **Already shared canonical Neon:** do not overwrite Neon with a stale VPS
  dump. Prove legacy and Next share the same tables/contracts, confirm migration
  versions and complete the fixture compatibility drill. Keep all committed
  web/bot writes in place during rollback; additive schema is retained.
- **Legacy data still on another database:** after all writers are frozen,
  take the final consistent restricted snapshot and copy/import using the
  reviewed mapping. Include web-owned tables and any Next-side data already
  present in Neon; conflicts must have a deterministic approved resolution.
  No drop/truncate/whole-database overwrite to make counts match.

The tooling section above records available versus planned commands. Execute
only the reviewed migration procedure bound to the recorded source/destination;
URLs are injected via approved secret bindings, never command arguments or
logged shell tracing. Retain source snapshot, destination pre-copy snapshot,
migration receipts, schema versions and hashes.

Verification must cover per-table counts **and** canonical per-key/content
checksums, snowflake/primary-key preservation, timestamps/time zones, JSON
settings, row constraints, tombstones/deletions, and pending/completed ledger
states. Aggregate counts alone are insufficient. Check pending unban deadlines,
schedule next-run times, audit/replay IDs, web read views and internal-action
idempotency. Destination verification has zero unexplained mismatches.

**Generated-key allocation is a separate pre-boot gate.** Explicit imported IDs
can leave a BIGSERIAL/identity sequence behind even when row checks pass. The
reviewed migration must inventory and reconcile **every imported generated-key
table's allocator**, including sequence/identity state and any application
allocator. Preserve imported and pre-existing destination IDs, allocated/reserved
high-water marks (including deleted IDs), and sequence `is_called`, increment
and bounds semantics; never rewind an allocator to the imported rows alone.
Record a per-table receipt proving the next allocation cannot collide or reuse
reserved IDs before releasing any gateway/web/job writer. Rehearse real default-ID
inserts/allocations on disposable fixtures, including nonempty destinations and
empty/deleted-high-water cases. Production verification/correction stays inside
the authorized migration procedure: no agent-run live test inserts, generic
`setval` recipe or allowlisted dump restore as a substitute. Missing coverage or
receipt is **NO-GO**. The same allocator gate applies to a rollback recovery target.

Source example: [events BIGSERIAL](../crates/cutover/migrations/0001_funnel.sql#L13)
is allocated by [gateway inserts](../crates/cutover/src/gateway_session.rs#L98)
that handle only idempotency-key conflicts, not primary-key collisions. The
[restore's sequence restart](../crates/core/src/backup/dump.rs#L215)
illustrates the hazard; it does not prove the planned copier covers every table.

A backup restore (the drill, or a rollback recovery target) meets this gate for
the archived tables only when the archive carries `sequenceMarks`: each
allocator resumes past the archived high-water (deleted IDs included), the
restored rows and the target's own position, and the CAS allocator moves past
every source token before rows are inserted. Record the receipt from the
manifest marks and the target's post-restore positions. An archive without
marks (frozen v3, or v4 written before marks existed) does **not** meet the
gate. Details: [backup runbook](backup.md).

Do not start either gateway while verification is unresolved. Record the baseline
watermark from which all subsequent Next/web writes will be reconciled.

## Registry swap, first boot and go/no-go

1. Confirm legacy is stopped, data verification is signed, producers stay
   frozen, and command restore artifacts are available to the executor.
2. Review `commands diff` against the **live snapshot**, including localization,
   options, default permissions, contexts and every scope. Follow the separate
   guild permissions recovery procedure **from its pre-PUT capture/reconciliation
   step**, including defaults and overrides on global commands. Apply only the
   reviewed reconciled registry through the authorized executor, map returned
   IDs and verify definitions/permissions/effective access. Record the receipts;
   do not use an ordinary bot start as an undocumented sync step.
3. Bind the existing application token and approved DB/signing secrets to
   Next through the secret service. Stage configuration while the gateway is
   stopped. First boot is a fresh IDENTIFY with **RESUME disabled**; never
   import legacy gateway session/sequence state. Ensure preflight/diff tooling
   has not already started a gateway; keep the Worker/DO fence active while
   staging configuration.
4. Reconfirm legacy is fenced and every data/registry/permission gate passed.
   The lead then authorizes release of the Next Worker/DO fence and startup of
   **one** Next Container. Record first READY, gateway budget remaining,
   process/gateway readiness, DB initialization and internal-action readiness.
   Check internal `/health` and `/readyz` and deployment/runtime logs. A 200
   proves only the components listed in its response, not every feature.
5. Compare real observed join/message/voice events with the moderator record,
   and verify agreed non-destructive command/web journeys. Do not generate
   moderation effects as test probes in production. Confirm due jobs/unbans
   are reconciled before releasing their single consumer. Release producers
   in a recorded order only after the lead's GO; record `T_0` when service is
   ready and open. RESUME may then be enabled only with Next's own persisted
   session, by the reviewed configuration path.

**GO requires:** no overlapping gateway/writers; final data/registry checks
match; preflight has no FAIL; fresh READY and all required components healthy;
no unexplained event gap, duplicate effect or missed deadline; all feature
sign-offs and rollback receipts present. Keep maintenance closed on uncertainty.

**Abort/rollback:** any auth/intent/permission failure, unmapped runtime behavior,
missed moderation action, unexplained data mismatch, duplicate side effect,
failed internal-action contract, crash loop, session budget exhaustion or inability
to maintain a durable rollback watermark. Budget exhaustion means wait for the
recorded reset/authorized recovery, not spin-restart or rotate the token.

## 48-hour watch

The lead records `T_0 + 48 h`, named coverage and a **real configured** monitor
before handing off. This document does not install one. Use short read-only
health polls on an internal endpoint (suggested 60 seconds), deployment events
and scheduled watch checkpoints at +15 min, +1 h, +6 h, +24 h and +48 h.
Report only findings; keep raw healthy polls in bounded restricted evidence.

| Signal | Finding / action |
|---|---|
| Process and gateway readiness | Sustained 503 past the measured recovery budget, restarts/crash loop or shard absent: freeze writers, investigate/rollback |
| Event continuity | Compare join/message/voice receipt to independent moderator observations; any unexplained gap or duplicated execution is a stop condition |
| IDENTIFY / RESUME | Record each reconnect, invalid session and current session-start budget; no restart loop consuming the recovery reserve |
| REST | Track 429/backoff compliance, 5xx/permission failures, action latency and uncertain sends; never blind-replay an uncertain Discord effect |
| Moderation / jobs | Pending and overdue unbans, due schedules, last-success times and failed claims; no overdue sanction without a named disposition |
| Data and web | Write/replay/claim conflicts, DB errors/pool pressure, internal-action contract failures, view drift and journal lag |
| Resources | RSS/placement versus B1 baseline, CPU/restart trend and storage/backup health; metrics are supplementary to event coverage |
| Rollback watermark | Durable captures cover every acknowledged write/effect; a gap immediately freezes new writes and fails GO |
| Registry / permissions | Record authorized administrator/tool edits and detect unexplained drift, including removed allows and changed defaults; preserve read snapshots for rollback reconciliation, not a baseline-only reset |

Use existing logs/receipts until metrics instrumentation is merged and wired;
do not assume `/metrics` or a feature counter exists. At +48 h record a
sign-off or extend the incident/watch on its execution card. Legacy remains
warm until separate retirement authorization; do not close B4 on a failed watch.

## Rollback: preserve Next-window writes before reopening legacy

**Maximum accepted loss:** **0 acknowledged committed database writes**.
The Next write window is `[T_0, T_r]` (up to the full 48-hour watch, longer if
extended), not merely the time since the last nightly backup. Restore-to-`T_f`
alone would lose that entire window and is **not an acceptable rollback**.
Durable shared data or complete delta reconciliation must cover it. Unacknowledged
in-flight work is fenced, retained and given an explicit disposition. Gateway
events missed while disconnected are a separate availability gap: record and
reconcile recoverable events; do not claim IDENTIFY will replay them.

1. Declare rollback and incident start time. Freeze **all** Next/web writers
   and producers again; do not start legacy yet. **Activate and verify the
   persisted Worker/DO ownership fence before stopping Next**, following the
   rehearsed procedure above. Pause Next health callers; fence pending keepalive,
   admitted fetches, `onStart` and every SDK auto-start path. Preserve queues,
   claims, replay IDs, gateway state and logs. Drain admitted work, stop Next,
   then use control-plane/log receipts to verify terminal state and no reconnect
   beyond the rehearsed keepalive horizon. An unfenced `/health` or `/readyz`
   request can restart Next and is not a stopped-state probe. Record `T_r` and
   final durable watermarks; preserve a restricted snapshot of Next data. Also
   fence registry/permission writers (including Discord administrator edits) and
   capture final live command definitions, guild permissions and defaults/
   inheritance **before any registry restoration**, as required above. Keep
   those writers fenced through reconciliation/read-back; DB snapshots cannot
   replace this external-state capture. Keep the Worker/DO fence active
   throughout legacy ownership. If fencing or either capture fails, keep
   maintenance closed; do not start legacy in uncertainty.
2. Reconcile **every write since baseline**, including config updates and
   deletes, XP/levels, onboarding state, tickets/transcripts, schedules/feeds,
   moderation actions/unbans, voice ownership, audit and internal-action/web
   ledgers. Use the tested per-table/key mapping and a transactionally consistent
   capture, not just `updated_at > T_f` (that misses deletes and some ledgers).
   Record counts/hashes/conflicts and the final applied watermark.
   - **Shared compatible Neon:** legacy returns to the same current data;
     validate its compatibility against the retained additive schema. Do not
     down-migrate, roll back Neon to an old snapshot, or restart legacy against
     its old divergent database.
   - **Separate legacy database:** reconcile into a recovered compatible target
     from the `T_f` baseline plus the full captured Next **and web** delta.
     Preserve primary keys, tombstones, sequence allocation, deadlines and
     completion/claim state. Include Next-created records with no original
     legacy counterpart. An unsupported mapping blocks reopening legacy; it
     does not permit dropping those records.
3. Discord effects already applied are **not undone by a database restore**.
   Use delivery/audit/replay receipts and authorized REST reads to classify
   completed versus uncertain work. Retain dedupe keys; do not replay completed
   messages, sanctions, role changes, unbans or web callbacks. Reconcile uncertain
   effects explicitly with moderators. Release/transfer leases only after old
   owners are fenced; drain overdue unbans once in the restored single consumer.
4. Reconcile the frozen final live registry/permission snapshot and approved
   watch-window edits with the pre-swap/post-swap receipts **before definition
   PUT**, following the separate guild permissions recovery procedure above.
   Apply the approved legacy-compatible **reconciled target**, preserving current
   restrictions/revocations and legitimate additions/deletions/renames, not a
   stale baseline reset. Complete old/current/restored ID maps, apply reconciled
   overrides via the authorized Bearer executor, verify defaults/inheritance and
   effective access, and read back every affected guild against that target.
   Definition PUT alone is insufficient. Unsupported drift, missing authority
   or any unexplained mismatch keeps commands frozen with a named disposition;
   never reinstate a role allow revoked during the watch to make old counts match.
   Global propagation/read-repair may leave stale clients temporarily, whereas
   guild commands update immediately. Report that gap, not a second gateway.
5. Restore pinned legacy image/config and the **reconciled** database binding.
   Preserve the existing application token. Read-only legacy preflight must
   pass; confirm the persisted Next Worker/DO fence remains active, Next is
   stopped, and session-start budget allows recovery.
   Start one legacy gateway with its tested fresh-session/reconnect procedure,
   never with Next session state. Record first READY and verify health, real
   event continuity, internal actions and pending jobs before reopening writes.
6. Resume producers/consumers once in the recorded order. Compare post-rollback
   watermarks and command registry, watch the recovered service for at least
   the measured drill recovery window, announce restored ownership and record
   incident/loss/gap/reconciliation evidence on B4. Keep Next evidence intact
   **and its Worker/DO fence active**; recovered monitors target legacy, not an
   auto-starting Next health route. Later deployment/retirement must not silently
   release the fence while legacy still owns the application.

If journal capture is incomplete or reverse reconciliation fails, keep affected
writes in maintenance, preserve both data sets, and escalate a decision brief to
the Director of Engineering. **Never silently choose a 48-hour data loss** to
restore availability. Any proposed loss/irreversible recovery requires its
separate authority; the lead cannot waive this runbook's zero-loss gate.

## Communication template

Use a private moderator channel for operational detail; public notice contains
no member IDs, token/config values, backup paths or permission-sensitive data.

```text
State: PLANNED | FROZEN | NEXT LIVE / WATCH | ROLLBACK | LEGACY RESTORED
UTC / lead / execution card: <time> / <name> / TOG-9699
Window: <start/end>; affected features: <list>; manual moderation contact: <name>
Gateway owner: <legacy stopped / next stopped / single owner>
Evidence: <release + gate + data/registry receipt links, no secret values>
Writes: <canonical DB + durable watermark>; pending actions: <count/disposition>
Decision: <GO / NO-GO / rollback reason>; loss: <zero verified / not yet verified>
Availability gap: <measured interval and event reconciliation status>
Next update: <UTC>; next actor/action: <name + exact step>
```
