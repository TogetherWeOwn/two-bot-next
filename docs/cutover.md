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
| Data | Current canonical database, every writer (bot, web, queues, cron), table/key ownership, migration versions, snapshot IDs/hashes, restore receipt |
| Registry | Global and each affected guild's complete command snapshots, intended diff, restore receipt |
| Secret bindings | Existing Discord token binding, database binding, internal-action signing binding and intended consumers; never values |
| Gates | Parity report, soak sign-off, W16 all-warm rollback drill, region/access sign-off, moderator sign-off |
| Window | Freeze time `T_f`, Next first ready `T_0`, rollback freeze `T_r` if any, watch deadline `T_0 + 48 h`, named on-call coverage |
| Recovery | Approved reconciliation mapping, journal/change-capture location, latest durable watermark, tested restore time and maximum loss window |

## Preconditions: all must pass

- [ ] `docs/parity.md` has **zero unmapped** legacy behaviors. Mapping is not
  implementation: every non-DROP behavior has merged runtime wiring and passing
  acceptance evidence, or an explicitly approved waiver. Moderation, automation,
  scheduled unbans and internal actions cannot be silently waived.
- [ ] [B2 soak](staging-soak.md) is signed off: seven consecutive days with zero
  missed gateway events, measured redeploy gap under 60 seconds and acceptable
  memory placement. Include feature/voice soak evidence, not just health polls.
- [ ] W16 all-warm rollback rehearsal has passed and shared Neon data/region,
  schema compatibility and backups are signed off. B4 runs **last**. The old
  migration plan's TypeScript/no-copy assumptions are not evidence for the Rust
  schema: resolve actual table compatibility before moving data.
- [ ] Candidate is merged, independently reviewed on its exact head, and
  `check` (fmt, clippy `-D warnings`, tests, cargo-deny), `worker check`, `pr-lint`
  and `gitleaks` are green on that head. Pin the resulting deployment digest.
- [ ] Each tool required below is merged into that candidate and its `--help`,
  safety behavior and rollback have been rehearsed with disposable fixtures.
  A planned subcommand is **not executable evidence**.
- [ ] Restore drill proves both database recovery and replay suppression, not
  just that a dump can be read. Data written by Next **and the web** during the
  watch can be read by legacy or reverse-reconciled without silent loss.
- [ ] Bot application identity is unchanged; web OAuth/internal-action callers
  use that application and the agreed endpoint/signing contract. Secret
  provisioning is by an authorized principal, under existing grants; new
  credentials/rotation require the applicable reserved gate.
- [ ] Command restoration is rehearsed for **both global and guild scopes**;
  permission overrides and command IDs are accounted for. Full replacement
  deletes omitted slash/user/message commands; no unrelated command is dropped.
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
session-start budget; FAIL is NO-GO, WARN needs a recorded disposition. Snapshot
legacy configuration and command registry *before* any overwrite. Confirm the
legacy restart path does not auto-register a different registry or resume a
stale session. Reconfirm first Next boot has RESUME disabled (see below).

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

Baseline checked: `44338b2` on 2026-09-30. Recheck the candidate source and
`--help` at execution time. The following are **planned, not merged into this
baseline**; do not paste them into a production shell or invent flags to make
them work:

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

For command snapshots/restoration, the authorized REST tool must cover:

```text
GET /applications/{application.id}/commands?with_localizations=true
GET /applications/{application.id}/guilds/{guild.id}/commands?with_localizations=true
PUT /applications/{application.id}/commands
PUT /applications/{application.id}/guilds/{guild.id}/commands
```

Keep raw GET snapshots and separately validated PUT payloads: response-only
fields and command-permission overrides require explicit handling; a GET JSON
file is not automatically a tested restore request. Restore every affected
scope, including user/message commands, not only slash commands. See the
[official Discord command API](https://docs.discord.com/developers/interactions/application-commands)
for bulk overwrite, localization and propagation semantics.


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
idempotency. Destination verification has zero unexplained mismatches. Do not
start either gateway while these are unresolved. Record the baseline watermark
from which all subsequent Next/web writes will be reconciled.

## Registry swap, first boot and go/no-go

1. Confirm legacy is stopped, data verification is signed, producers stay
   frozen, and command restore artifacts are available to the executor.
2. Review `commands diff` against the **live snapshot**, including localization,
   options, default permissions, contexts and every scope. Apply only the
   reviewed full desired registry through the authorized command executor.
   Verify the returned registry and retained permission overrides. Record the
   receipt; do not use an ordinary bot start as an undocumented sync step.
3. Bind the existing application token and approved DB/signing secrets to
   Next through the secret service. Stage configuration while the gateway is
   stopped. First boot is a fresh IDENTIFY with **RESUME disabled**; never
   import legacy gateway session/sequence state. Ensure preflight/diff tooling
   has not already started a gateway.
4. Start **one** Next Container. Record first READY, gateway budget remaining,
   process/gateway readiness, DB initialization and internal-action readiness.
   Check internal `/healthz` and `/readyz` and deployment/runtime logs. A 200
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
   and producers again; do not start legacy yet. Preserve queues, claims,
   replay IDs, gateway state and logs. Drain admitted work, stop Next and
   verify no replica/supervisor can reconnect it. Record `T_r` and final durable
   watermarks; preserve a restricted snapshot of Next data. If the DB is
   unavailable, leave writers stopped until capture/recovery is possible.
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
4. Restore the complete legacy command registry in each affected global/guild
   scope from the reviewed restore payload. Verify payload, IDs and permissions;
   global propagation/read-repair may leave stale clients temporarily, whereas
   guild commands update immediately. Report that gap, not a second gateway.
5. Restore pinned legacy image/config and the **reconciled** database binding.
   Preserve the existing application token. Read-only legacy preflight must
   pass; confirm Next is stopped and session-start budget allows recovery.
   Start one legacy gateway with its tested fresh-session/reconnect procedure,
   never with Next session state. Record first READY and verify health, real
   event continuity, internal actions and pending jobs before reopening writes.
6. Resume producers/consumers once in the recorded order. Compare post-rollback
   watermarks and command registry, watch the recovered service for at least
   the measured drill recovery window, announce restored ownership and record
   incident/loss/gap/reconciliation evidence on B4. Keep Next evidence intact.

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
