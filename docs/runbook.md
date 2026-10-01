# two-bot-next operations runbook

For the on-call operator of the Rust bot and its Cloudflare Worker/Container.
This describes the shipped source, not proof that an environment is deployed,
that a soak passed, or that production cutover is approved. Cutover, production
restores, token rotation, and live-guild changes need their separate authorization.

## Start here

1. Confirm the affected environment, reviewed Git SHA, Worker version ID,
   container image, time of last good readiness, and incident reference.
2. Check **both** liveness and readiness; capture their component breakdown.
3. Read Worker **and container** logs. Contain the problem before redeploying;
   do not turn on an unwired feature in an attempt to repair it.
4. For a regression, select a known-good version/image pair compatible with the
   current database schema. Follow [redeploy/rollback](#redeploy-and-rollback).
5. Record the resulting deployment/version/image and first ready time. A command
   returning success alone does not prove recovery.

All npm commands below run **from the repository root** and use the pinned
Wrangler 4.143.1 through `wrangler/package.json`. Use only the already-authorized
Cloudflare connection. An authentication/permission failure is a stop: report
it, do not try another credential, elevate access, or use a personal token.
Examples target `staging`; do not substitute `production` without its own gate.
`STAGING_WORKER_URL`, `VERSION_ID`, and `CONTAINER_APPLICATION_ID` are operator
inputs from the approved environment/deployment record, not invented defaults.

```bash
npm --prefix wrangler ci --include=dev
two-bot --help
```

The installed binary is named **`two-bot`**, not `two-bot-next`. With no arguments
it runs the gateway/HTTP service; there is **no `serve` or `restart` subcommand**.
Use the top-level help above: backup subcommands do not implement per-command
`--help`, despite the old comment in `backup_cli.rs`.

## Is it alive?

```bash
curl --silent --show-error --max-time 10 --include "${STAGING_WORKER_URL}/health"
curl --silent --show-error --max-time 10 --include "${STAGING_WORKER_URL}/readyz"
```

These preserve the HTTP status and body, including a useful 503 breakdown.
They are HTTP observations, **not database probes or test authorization**.

| Observation | Meaning / next action |
|---|---|
| `/health` 200, `{"status":"ok"}` | The process can answer HTTP. Not proof of Discord, database, feature services, or end-to-end delivery. |
| `/readyz` 200, components include `process=ready`, `gateway=ready` | READY/RESUMED dispatch has committed. These are the two status-gating components; the `jobs` object is informational and can report failures even while ready. |
| `/readyz` 503, gateway `down` | Missing gateway prerequisites; service is parked. Check binding **names**, not values. |
| `/readyz` 503, gateway `starting` | Connecting/reconnecting or bounded checkpoint I/O. Compare duration with logs; persistent 503 is not healthy operation. |
| Neither route answers / 500 / HTTP 1101 | Inspect Worker bindings and container startup. Named environments must repeat all Container/DO/exports wiring; do not bypass readiness. |

The container listens on `LISTEN_ADDR` (default `0.0.0.0:8080`). The Worker
sets it from `BOT_PORT` (default 8080). Both routes use this listener; there is
no legacy health port 9191 or separately wired DB/voice readiness component.
For an operator already inside the authorized container, its Docker liveness
probe is also available:

```bash
two-bot --healthcheck
```

It probes local `/health` and exits 0 for HTTP 200, 1 otherwise. This is **not**
a readiness check. The Worker routes these two paths to singleton `two-bot`;
other paths serve the invite redirect, not a bot admin API. The redirect's
`/healthz` can return 200 without starting the bot; **never use it as gateway
health**. The binary exposes private `/metrics`, but the Worker and DO do not
proxy it; use only an already-authorized internal scrape, never a public route.
Internal-action endpoints and `/voice/ownership/health` are not wired bot endpoints.

Source: [`server.rs`](../crates/bot/src/server.rs),
[`health.rs`](../crates/core/src/health.rs),
[`main.rs`](../crates/bot/src/main.rs),
[`wrangler/src/index.ts`](../wrangler/src/index.ts).

### Logs and keepalive

```bash
npm --prefix wrangler run logs -- --env staging --format pretty
npm --prefix wrangler run containers -- list --env staging
npm --prefix wrangler run containers -- info "${CONTAINER_APPLICATION_ID}" --env staging
npm --prefix wrangler run containers -- instances "${CONTAINER_APPLICATION_ID}" --env staging
```

`logs` is **Worker/DO tail**, not Rust stdout. Stop it when the bounded incident
observation is complete. For Rust stdout/stderr, use the affected container's
logs in the Cloudflare dashboard. Wrangler 4.143.1 has no `containers logs`
subcommand; do not invent one. Container inspection may list account-wide
resources: match the affected environment/application before taking any action.

Rust uses formatted `tracing` logs, configured by `RUST_LOG`, fallback
`two_bot=info`; it does not consume legacy `LOG_LEVEL`. This wrapper currently
forwards **only** `DISCORD_TOKEN`, `DATABASE_URL`, `GUILD_ID` and its computed
`LISTEN_ADDR`, not `RUST_LOG` or arbitrary `TWO_*` flags. Adding a Worker var
alone will not configure the container. Do not dump env or HTTP headers to
troubleshoot; redact tokens, connection strings, and member data from evidence.

Look for these literal messages:

- `listening` — HTTP listener bound.
- `gateway prerequisites missing; gateway parked, /readyz reports down` — first
  missing runtime binding named, shard not running.
- `durable gateway initialized; shard connecting` (`resume=true|false`) — startup
  loaded its durable state; not yet proof of a successful RESUMED event.
- `gateway shard loop started` / `gateway reconnect failed; Twilight will retry`.
- `durable gateway failed; checkpoint unchanged, readiness unavailable` — fatal
  initialization failure; underlying SQL error deliberately not logged.
- `container service failed` / `SIGTERM received; draining`.
- Worker: `two-bot container started|stopped`, `two-bot /readyz unhealthy`,
  `two-bot keepalive probe failed`, `two-bot container error`.

The DO renews activity and probes `/readyz` every `KEEPALIVE_SECONDS` (default
60); `sleepAfter` is 30 minutes. Outbound gateway traffic alone does not keep
an idle container awake. Do not disable the keepalive or increase capacity
without measured evidence. `lite`, `max_instances=1` is the declared placement,
not evidence of the measured RSS budget. See [staging soak](staging-soak.md).

## Redeploy and rollback

### Before changing anything

- Use a clean checkout of the approved revision; tests and exact-head CI must
  be green. No second bot instance/shard for the same guild.
- Record current deployment and known-good **Worker version + Git SHA + image**.
  Versions are not Git SHAs. Check whether migrations or resource changes make
  the old code incompatible; rollback does not rewind Postgres or DO storage.
- Check the current staging workflow result. `deploy-staging.yml` runs on merge
  to `main`, and also allows manual dispatch, but currently accepts a 503
  readiness response as a scaffold-era gate. **Workflow green is not gateway
  ready**: require your own first `/readyz` 200 observation and feature evidence.
- Gateway startup connects DML-only and does not migrate. **The lazy periodic
  jobs path currently requests migrations and web-contract DDL**; do not infer
  the whole runtime is DML-only. Confirm schema compatibility with its owner
  before redeploying; a DML-only login may leave jobs failing `database` while
  gateway-ready. This does not authorize extra grants, manual SQL, restores,
  migrations or migration tests on staging/production.

```bash
npm --prefix wrangler run deployments -- list --env staging
npm --prefix wrangler run versions -- list --env staging
npm --prefix wrangler run versions -- view "${VERSION_ID}" --env staging
```

Wrangler lists the ten most recent versions/deployments. Select the actual
previously healthy version from your deployment record; never silently choose
"latest" or omit the rollback ID.

### Redeploy the approved revision

With the correct revision already selected in a separate clean operator checkout,
installed dependencies, and Docker available to build `../Dockerfile`:

```bash
npm --prefix wrangler run deploy -- --env staging
```

This is the same full Worker + Container deploy used by staging CI. Keep named
environment bindings explicit. Do **not** use versions upload/deploy as a
substitute for a full container-image deploy. Deployment is not transactional:
Worker activation can succeed while a later image/rollout step fails. Recheck
both Worker version and running image, then `/health`, `/readyz` and startup
logs; record finish-to-first-ready gap (soak target under 60 seconds). A container
replacement can restart the shard; there is no promise of zero downtime.

### Worker-version rollback

For a Worker-only regression with compatible current image/resources:

```bash
npm --prefix wrangler run rollback -- "${VERSION_ID}" --env staging
npm --prefix wrangler run deployments -- list --env staging
```

Read the interactive target and confirm the incident's known-good version.
Do not add `--yes` or override warnings. If Wrangler reports changed secrets,
DO lifecycle changes, missing bindings, or an access denial, **stop**; do not
force the rollback or revive/replace credentials. Escalate the compatibility
or authorization decision with names and the error code, never secret values.

**Worker rollback is not a container-image or data rollback.** The pinned
Wrangler rollback handler creates a Worker deployment; it does not rebuild an
old image or run the full Container rollout path. If the Rust image is bad,
redeploy the known-good reviewed source/image pair from its clean checkout
using the full `deploy` command above, only after confirming it supports the
current schema and bindings. Confirm the running image separately. If no safe
pair is known, contain and escalate instead of guessing.

After either recovery: repeat the health/readiness/log observations, confirm
only one gateway session, and record version, image, first ready time and
remaining limitations. None of these examples were a live rollback drill.

Cloudflare references:
[Worker rollbacks and resource limits](https://developers.cloudflare.com/workers/versions-and-deployments/rollbacks/),
[Container rollout semantics](https://developers.cloudflare.com/containers/configuration/rollouts/).
Local contract: [`wrangler.toml`](../wrangler/wrangler.toml),
[`package.json`](../wrangler/package.json),
[`deploy-staging.yml`](../.github/workflows/deploy-staging.yml).

## Restart semantics: durable RESUME, not full state recovery

The normal service invocation is:

```bash
two-bot
```

Do not run this beside an already-running gateway; restart/replacement belongs
to the authorized container supervisor or the deploy procedure above. SIGTERM
or SIGINT drains HTTP and cancels the gateway task. A configured gateway task
failure is process-fatal (exit 1); external supervision owns restarting it.

Startup requires `DISCORD_TOKEN`, `DATABASE_URL`, and a nonzero numeric `GUILD_ID`.
`DATABASE_URL` must point at the runtime login after the operator has applied
migrations: the gateway connects Postgres (fixed pool maximum 5) with a DML-only
identity and never migrates at startup. It loads
funnel milestones, and reads `gateway_sessions` for **guild + shard 0**. A saved
session ID, sequence and resume URL are supplied to Twilight when the checkpoint
is valid and at most **15 minutes old** (inclusive). Missing, empty, expired or
future-dated checkpoint means IDENTIFY instead; validity is not a guarantee
that Discord will accept RESUME.

Funnel effects, member projections/activity and checkpoint advancement commit
in one transaction. Same-session sequences at/below the committed sequence are
duplicates, not replayed effects. Checkpoint I/O is bounded by the smaller of
5 seconds and one-quarter of the HELLO heartbeat interval, with readiness
`starting` while it runs. Initialization/hydration has a different budget.

Close codes **4007/4009** discard the checkpoint and construct a fresh shard;
opcode 9 `d:false` also clears durable state. Twilight owns transport fallback.
Do not clear sessions or manipulate sequences by hand. `resume=true` on startup
only proves an attempted restore; `/readyz` 200 follows a committed READY or
RESUMED. Capture the gap and evidence instead of claiming every restart RESUMEs.

Only first/second/third-message and first-voice-session milestones rehydrate.
Caches and open voice durations are not full durable recovery; READY/RESUMED
clears open voice sessions. There is no operator command to restore them.
Source: [`gateway.rs`](../crates/bot/src/gateway.rs),
[`gateway_session.rs`](../crates/core/src/gateway_session.rs),
[`durable store`](../crates/cutover/src/gateway_session.rs),
[recovery notes](gateway-recovery.md).

## Containment, kill switches and feature flags

**Do not confuse a ported core contract with an active control.** The current
binary includes gateway/cache/funnel, the shared command runtime (sticky/feed
slices), and supervised website/community jobs subject to their startup gates.
It does not start audit delivery, automod sanction workers, moderation action
handlers, internal-action HTTP or the settings poller. The Worker does not
forward the feature vars. There is no binary audit-halt command, persistent
Worker pause or hot-reload/admin endpoint to recommend. See the
[incident playbooks](#incident-playbooks) for the actual degradation and stop
boundaries; registry entries do not prove all command slices execute.

For unexpected writes: first identify the actual writer (legacy bot, next image,
other automation). Preserve evidence and use the authorized deployment/containment
procedure for that writer. A legacy flag or a SQL row is **not** a verified
next-runtime kill switch. Do not issue SQL against live/staging services, switch
tokens, or redeploy with an unreviewed wiring change during this docs procedure.

| Control | Ported contract | Current operational boundary |
|---|---|---|
| Audit kill switch | Durable `audit_kill_switch`, singleton `id=1`; `engage_halt(actor_id)` / `disengage_halt()` library methods. Halt blocks queue discovery/claim/preparation, not fact recording or an already prepared/in-flight send. | Store exists, delivery service/operator endpoint not wired. No supported command in this binary. Durable DB errors propagate; legacy pure-model read-fail-open is **not** send authorization. See [audit store](audit-store.md). |
| Automod | Exact `TWO_AUTOMOD=1` enables config. Dry-run by default; exact `TWO_AUTOMOD_ENFORCE=1` selects enforce. | Matcher/config only, no runtime enforcement. Setting enforce does not activate sanctions. `TWO_AUTOMOD=1` **does** request MESSAGE_CONTENT intent for direct binary execution. |
| Moderation | Exact `TWO_MODERATION=1`; enabled config requires `TWO_OWEN_USER_ID`; protected-role policy applies. | No running moderation command handler. No config toggle demonstrated as an emergency stop. |
| Automations/announcements/text | `TWO_AUTOMATIONS=1`, `TWO_ANNOUNCEMENTS=1`; text needs automations **and** `TWO_TEXT_COMMANDS=1`. | These action slices are library gates; shared registry publishing and periodic jobs exist, but do not make these gates operational containment. |
| Onboarding | `TWO_ONBOARDING_MODE=legacy|session|anchor`, default legacy; `TWO_ONBOARDING_DRY_RUN=1`. | Core only; session/anchor roles and dry-run are not active runtime switches. |
| Community scorecard | `TWO_COMMUNITY_SCORECARD=1`; recommendations on unless `TWO_COMMUNITY_RECOMMENDATIONS=0`. | Conditional supervised scheduler is wired; weekly attempt is consumed before DB work. Worker does not forward these flags; no hot stop switch is demonstrated. |
| Internal actions | Moderation requires `TWO_INTERNAL_ALLOW_MODERATION=1` **and** `TWO_MODERATION=1`; other verbs have allow flags. | Durable replay store/executor ports do not create an HTTP listener or authorize writes. |
| Settings hot reload | Typed catalogue/store with env-only secret/moderation keys. | Poller/runtime rebuilding remains follow-up; no promise of changes applying without restart. |

Source: [`automod.rs`](../crates/core/src/automod.rs),
[`moderation.rs`](../crates/core/src/moderation.rs),
[`feature_commands.rs`](../crates/core/src/feature_commands.rs),
[`onboarding.rs`](../crates/core/src/onboarding.rs),
[`settings.rs`](../crates/core/src/settings.rs),
[`pipeline.rs`](../crates/discord/src/pipeline.rs). Do not enable new writes until
runtime wiring, environment propagation, containment and staging evidence exist.

## Backup, restore and drill commands

[Backup procedures and formats](backup.md) are the detailed contract. These
commands are **rehearsals on agent-testdb/local fixtures only**; never point
tests, probes, dry runs with a DB URL, or restore verification at production
or staging databases. No production restore is authorized here.

For a prepared, separately named test database carrying the required schema,
with `TWO_BACKUP_DIR`, retention and an approved upload wrapper already configured:

```bash
TWO_DATABASE_URL=postgres://agent_test@agent-testdb:5432/two_next_backup_test two-bot backup
two-bot backup-upload "${BACKUP_FILE}"
env -u TWO_RESTORE_URL two-bot restore "${BACKUP_FILE}" --dry-run
TWO_RESTORE_URL=postgres://agent_test@agent-testdb:5432/two_next_restore_drill two-bot restore "${BACKUP_FILE}" --force
```

`BACKUP_FILE` is the actual published `two-funnel-*.ndjson.gz`, not a partial
file. `backup` uses **`TWO_DATABASE_URL`**, not the gateway's `DATABASE_URL`.
It validates before publication, pruning or upload; an empty event log fails
and skips prune/upload. Retention defaults to 14. An unset upload command warns:
exit 0 alone is **not proof of off-box recovery**. `backup-upload` is a real
external PUT; run only with the specifically authorized test backup destination
and provisioned S3/R2 credentials. Never copy data to an invented bucket.

The `env -u` example makes dry-run a file-only check: an inherited
`TWO_RESTORE_URL` would otherwise trigger a database probe. Require exit 0 plus
`DRY RUN VERIFIED`, or for the destructive scratch restore `RESTORE VERIFIED`
and matching per-table counts. `--force` only confirms intent; it is not
authority to use a non-test target. The restore command **does not migrate**;
the scratch target must already have the schema, despite the old dry-run
output mentioning migration. Unknown restore options refuse (exit 2).

Guild-config capture/restore is different: it talks to Discord. Do not run it
with live tokens or as a database test. Before either command, the authorized
operator must have `DISCORD_STAGING_GUILD_ID` set from the approved TWO Staging
guild configuration/deployment record, plus the provisioned Owen QA Test staging
credential described below. This non-secret identifier is independently required
and checked against the pinned TWO Staging guild by
[`staging_guild_id`](../crates/core/src/backup/guild_config.rs); gateway `GUILD_ID`
is **not** a substitute. A missing or mismatched staging identifier refuses before
capture/planning. This is not authorization to provision a guild or credentials.
With separate authorization for that staging guild/identity, the available
operator commands are:

```bash
two-bot guild-config-snapshot
two-bot guild-config-restore --snapshot "${SNAPSHOT_FILE}"
two-bot guild-config-restore --snapshot "${SNAPSHOT_FILE}" --confirm-staging-guild --apply --evidence "${EVIDENCE_FILE}"
```

Planning performs live identity/capture reads, even without `--apply`.
Snapshot requires upload of both sealed snapshot and drift report. Tampering
refuses with exit 3 before Discord access; legacy unsealed input warns and
lacks that protection. Apply requires the staging confirmation flag and current
permissions/hierarchy; require converged hash and exit 0, not just `DID` lines.
The credential-file loader refuses empty/invalid/unreadable input without
substitution; the documented environment fallback is only for an absent file.

The three shipped service/timer pairs are **operator-host templates**, not
installed Cloudflare schedules: nightly DB backup 04:17, guild-config backup
04:31 UTC, monthly scratch restore drill on the 1st at 05:30. Local-container
disk is ephemeral; do not treat these files as evidence of off-box retention
or an installed timer. Host installation/actions require the authorized broker
or existing operator handoff; see [backup.md](backup.md) for unit contracts.

## Common failures

| Symptom | Check / safe next action |
|---|---|
| Token missing / invalid | Missing `DISCORD_TOKEN` parks the gateway. A present rejected token can produce generic gateway failure rather than a dedicated invalid-token log. Confirm the expected secret **name/environment** with its provisioner; stop on rejection. Do not use legacy `DISCORD_BOT_TOKEN` as an automatic replacement or rotate credentials in this procedure. |
| Missing Discord intents | GUILD_MEMBERS is always requested. MESSAGE_CONTENT is conditional on automod or all three ticket identifiers. Check the intended bot's Developer Portal intent grants and runtime configuration through the authorized actor; no speculative privilege expansion or token switch. There is no separate intents health component. |
| DB unreachable / migrations fail | `DATABASE_URL` is required. Gateway connect/hydration/checkpoint failure can exit the process; its SQL error is withheld. The separate lazy jobs path requests migrations/DDL and can fail `database` while ready stays green. Follow the [Neon/Hyperdrive playbook](#neon-or-hyperdrive-outage); fix the named dependency through its owner, not another credential, SQL probe or widened grant. |
| HTTP 200 health but persistent 503 ready | Listener works, gateway does not. Read `gateway` state and startup logs. Never soften readiness or count the scaffold-era deploy gate as recovery. |
| Reconnect / RESUME refused | Follow [restart semantics](#restart-semantics-durable-resume-not-full-state-recovery); 4007/4009 force fresh IDENTIFY. Preserve the durable checkpoint, don't hand-edit sequence or start another shard. |
| Discord REST 429 / suspected breaker | Command/jobs REST executors are wired, but no shared token-wide admission governor/breaker is active. Per-executor pacing is not global containment; sticky/interaction sends use single unpaced attempts and later sticky activity can retry. See the [Discord playbook](#discord-gateway-or-api-outage) for retry/admission boundaries; do not hammer Discord, replay uncertain writes or invent a reset command. |
| Ready but feature inactive | Gateway readiness says nothing about library-only commands/jobs/kill switches. Check [runtime boundaries](#containment-kill-switches-and-feature-flags), not extra environment guesses. |
| Worker restored but Rust regression remains | Worker-version rollback did not prove image rollback. Inspect the active image and use a schema-compatible full redeploy of the known-good pair. |
| Backup/drill red | Preserve valid archives; inspect exit status, verifier line, table counts and off-box receipt. Rehearse only on a prepared test database. No automatic promotion to a production restore. |

## Incident playbooks

Source contracts checked at `8af9b23be40935b97cf29512be3aa45a8b368858`;
re-check the affected deployment if its revision differs. This is not proof of
a staging/production deployment or live recovery.

Use these procedures for the **affected, verified environment** only. They do
not authorize production deployment, a DB restore, an outage injection, or any
credential change. Record UTC, incident reference, last good observation,
reviewed Git SHA, Worker version and running image before acting. Do not collect
raw tokens, DB URLs, request headers, gateway session IDs or member payloads.
An access denial is a stop condition, not evidence of a provider outage: report
the principal, operation, non-secret target and error to the engineering manager;
never change credentials or bypass the gate.

A dry-run walkthrough is non-disruptive: observe the authorized staging
baseline, walk the hypothetical outage branches below, and record what evidence
would permit recovery. Do not actually disconnect Discord, change a Neon/
Hyperdrive binding, delete a checkpoint, send a moderation action, or stop the
staging container. [Tabletop evidence](incident-tabletop-2026-10-01.md) separates
local source rehearsal, actual staging observations and unfinished acceptance.
A denied observation or successful offline test is not a completed staging drill.

### Discord gateway or API outage

**Detection.** Compare `/health` with the `gateway` component of `/readyz`.
Gateway reconnect symptoms and REST send failures are different incidents:
gateway-ready does not prove REST delivery, and HTTP liveness does not prove
either. Use literal `gateway reconnect failed; Twilight will retry`,
`durable gateway initialized; shard connecting` (`resume`) and
`gateway ready; checkpoint committed` (`sequence`). Worker
`two-bot /readyz unhealthy: <status>` is a readiness observation, not a Discord
cause. Do not infer a Discord outage from a Worker error or 403 alone.

Existing **private** metrics are `two_bot_gateway_reconnects_total`,
`two_bot_gateway_resumes_total`, `two_bot_gateway_latency_seconds` and
`two_bot_rest_requests_total{route,result}` (`429`, `5xx`, `transport` distinguish
REST symptoms). Use an existing authorized internal scrape, not the public
Worker: [metrics exposure](metrics.md) documents that `/metrics` is not proxied.
Counters reset on process replacement; reconnects count subsequent HELLOs, not
failed dials; RESUMED is counted **before** its durable commit; latency can be
`NaN`. None is a gateway-ready/admission gauge or a breaker-reset control.

**First five minutes.**

1. Confirm environment, singleton `two-bot`, affected guild and deployment
   provenance. Save status/body of both health routes using [health checks](#is-it-alive).
   Capture the first failing time and last ready time, not just a screenshot of
   a current green response.
2. Correlate reconnect/send evidence with
   [Discord status](https://discordstatus.com/). Classify gateway transport,
   REST 429/5xx/timeout, rejected authentication, missing intents, or local
   regression separately. A rejected credential follows the token/provisioner
   path, not a retry with another token.
3. Let Twilight handle transport reconnection. On startup a valid checkpoint
   at most 15 minutes old permits attempted RESUME; invalid/expired state uses
   IDENTIFY. Codes 4007/4009 and non-resumable invalid session clear state in
   code and force fresh identification. Do not force either mode, delete a
   checkpoint, edit a sequence, start a second shard, or loop manual restarts.
4. Determine which actual runtime is sending REST requests. Do not assume a
   library cooldown or unmerged admission governor is active protection. Stop
   operator-initiated send/replay work; preserve ambiguous outcomes rather than
   repeating moderation, announcement or role writes.

Current send admission is **per executor**, not shared per token: command and
job executors are separate; clones share only their executor's pacing state.
The paced floor is 110 ms (kick floor 350 ms). Sticky message POST and interaction
callback/edit use single five-second attempts without that pacing path; a sticky
POST failure releases its claim so later message activity can try again.
Registry publication has at most five sends for repeated 429/5xx. Paced job GET
429 retries do not consume the attempt count and rely on the outer job deadline.
There is no wired token-wide cooldown/queue/breaker to reset or rely on for
containment. Internal announcement cooldown receipts are a library contract,
not active admission. Preserve uncertainty rather than promising no later retry.

**Containment.** Keep the durable checkpoint and event evidence intact. Respect
server retry-after and the executor's bounded delay rather than increasing
traffic during a 429. No manual breaker reset, request burst, second identity,
or credential substitution. If a local regression is proven, use only the
schema-compatible reviewed [rollback procedure](#redeploy-and-rollback).
A provider outage alone is not a reason to redeploy. If bot-wide isolation is
necessary, use the Worker-side containment prerequisite in the
[token playbook](#suspected-bot-token-compromise-containment-only); a one-time
process stop is not a persistent pause.

**Recovery verification.** Require actual `/health` 200 and `/readyz` 200 with
`process` and `gateway` ready after committed READY/RESUMED. `resume=true` alone
is an attempt, not success. Record the first ready UTC and outage/reconnect gap;
confirm one session/instance and no continuing reconnect/fatal failures over a
bounded observation window (at least two configured keepalive intervals).
Only under separately authorized staging E2E, perform one known feature journey
and verify its effect; do not create a live-guild test message or moderation
write as a health probe. REST recovery needs evidence of completed delivery,
not only gateway readiness. Preserve unknown send outcomes for reconciliation;
do not bulk-replay them. No claim that caches, open voice durations or all
missed events are restored by RESUME/IDENTIFY.

**Post-incident record.** Save affected surface, onset/last good/first ready UTC,
version/image/SHA, gateway mode (attempted RESUME versus verified recovery or
fresh IDENTIFY), close/status codes if available, provider incident link,
redacted evidence, admission/retry decisions, uncertain effects, lost-event/
voice-duration limitations, verification and follow-up owner. Separate observed
facts from inferred cause and from the hypothetical tabletop.

Source: [gateway and durable recovery](gateway-recovery.md),
[`gateway.rs:195–319`](../crates/bot/src/gateway.rs),
[`gateway_metrics.rs:19–68`](../crates/bot/src/gateway_metrics.rs),
[`metrics.rs:196–253`](../crates/core/src/metrics.rs),
[`command_runtime.rs:461–474`](../crates/bot/src/command_runtime.rs),
[`executor.rs:490–568,820–851,1347–1429`](../crates/discord/src/executor.rs),
[Discord connection lifecycle](https://docs.discord.com/developers/events/gateway#connections).

### Neon or Hyperdrive outage

**Detection.** Gateway Postgres uses the forwarded `DATABASE_URL` directly.
Hyperdrive `REDIRECT_DB` is a separate declared redirect binding, **not** the
Rust connection path. At this source revision, the Worker constructs its
`RedirectStore` with an `undefined` connector: it serves the configured snapshot
and silently drops non-live click records even if the Hyperdrive binding exists.
Do not diagnose lost attribution as a new Hyperdrive outage when live persistence
was never wired, or claim a working redirect proves either DB is healthy.

Use literal `durable gateway failed; checkpoint unchanged, readiness unavailable`
(no underlying SQL error), `periodic job failed` (`job`, `error_class=database`),
and `sticky lookup failed; skipping activity` /
`sticky claim failed; skipping activity` (`error`, redact before sharing).
The informational
`jobs` readiness object records `last_error_class`, `consecutive_failures` and
`last_success` (epoch **milliseconds**); job failures do not change the HTTP
ready status. Existing private metrics
`two_bot_job_last_success_timestamp_seconds{job="session_checkpoint"}` (epoch
**seconds**) and `two_bot_db_pool_configured`, `two_bot_db_pool_connections`,
`two_bot_db_pool_idle_connections`, `two_bot_db_pool_max_connections` are hints,
not DB connectivity probes. The gauges sample only the gateway pool, not the
lazy job pool; the six periodic jobs do not populate that success metric.
Use only existing authorized [internal metrics](metrics.md); never proxy them
publicly. There is no DB-ready component or DB-error metric. Preserve actual
health/readiness and sanitized evidence, not URLs or unredacted SQL errors.

**First five minutes.**

1. Identify the affected staging dependency from approved non-secret deployment
   metadata: [staging configuration](staging-soak.md#provisioning-operator-once)
   records dedicated `two_bot` DB/role on Neon staging, separate from the web's
   `two`/`two_app`. Never derive a replacement URL from another application's
   secrets. The deployed binding must be confirmed, not merely assumed from
   this configuration document.
2. Save both bot health responses and available sanitized startup/checkpoint
   logs. Check [Neon status](https://neonstatus.com/) and
   [Cloudflare status](https://www.cloudflarestatus.com/) alongside the actual
   dependency owner's evidence. Missing schema/grants, rejected credentials,
   startup failure and provider unavailability require different repairs.
3. Treat inability to persist gateway events/checkpoints as unsafe operation,
   not a license to continue on in-memory state or skip a transaction. Keep
   stored state intact. Gateway-ready does not prove every feature store is
   available; pause manual/operator writes and investigate the specific writer.
4. Check the **deployed** redirect wiring before treating Hyperdrive as an
   active dependency. Current source is snapshot-only: no live lookup/write
   occurs, so there is no Hyperdrive recovery to test through this Worker yet.
   If a later reviewed deployment wires a live connector, distinguish lookup
   fallback from failed click attribution using that revision's evidence.
   Do not call redirect `/healthz` proof of gateway recovery or manufacture a
   campaign click to exercise a production DB.

**Containment.** The dependency owner repairs the existing target/network/
schema/binding through their authorized procedure. No extra grant, credential
substitution, URL swap, migration, checkpoint deletion, DB reset or restart
storm. If configured gateway persistence fails, allow its fail-closed termination
rather than softening readiness or launching another writer. Do not claim all
command effects are transactional or rolled back merely because a checkpoint
failed. Worker HTTP, fallback redirects and bot persistence have separate
failure boundaries. Startup DB/checkpoint/milestone load failure prevents the
essential gateway task from starting; persistence failure in its running loop
ends that task and supervision exits the process. There is no durable offline
spool. Sticky lookup/claim DB failures skip reposting; sticky/feed command
failures produce failure replies. Command tasks are detached **before** the
checkpoint commits, so not every Discord effect belongs to that transaction.

Periodic `counter`, `rank`, `scheduled_events` and conditionally registered
`presence_probe`, `community_scorecard`, `inactivity` jobs can instead fail
`database` while gateway readiness stays green. Ordinary job failure is recorded
and cadence continues, but the scorecard consumes its weekly attempt before
DB work, so do not promise an immediate same-week retry. **Gateway connect is
DML-only, but the lazy jobs connection currently requests migrations and the
web contract DDL.** A DML-only login may therefore leave jobs failing even with
a ready gateway. Route this schema/grant discrepancy to the dependency/runtime
owner; this runbook does not authorize widening grants or executing migrations.

Retain the last verified off-box archive and its receipt; an outage is not proof
of corruption. The v3 [backup contract](backup.md) covers the **22 allowlisted
bot tables**, including funnel/projections, not the whole evolving runtime DB.
[`DUMP_TABLES:44–67`](../crates/core/src/backup/dump_file.rs) does not include
`gateway_sessions` or website tables. It is not a full Neon, Discord or
Hyperdrive configuration restore.
Never restore over the affected DB as a connectivity fix. File-only archive
verification and a prepared **agent-testdb** restore rehearsal are available in
[backup/restore](#backup-restore-and-drill-commands); no staging/production restore
or new backup destination is authorized. A real data-loss recovery needs its
separate decision, schema-compatible target, known archive age/counts and
recorded RPO/RTO, not a guess at the newest filename.

**Recovery verification.** Have the dependency owner confirm the intended
binding/target and repair receipt without exposing credentials. Then observe
health 200, ready 200 with actual component breakdown and no continuing fatal
persistence failures over at least two keepalive intervals. A separately
authorized staging journey must confirm the affected feature's persisted
outcome; readiness alone is insufficient. For this snapshot-only redirect,
record the unwired live-attribution boundary rather than claiming Hyperdrive
recovery. A later live connector requires its own lookup/attribution evidence.
Do not use a SQL test, migration or destructive restore as the verification step.

**Post-incident record.** Save the affected direct-DB/Hyperdrive surface,
non-secret environment/branch/binding identity, safe log fields, last committed/
first recovered evidence when available, deployment provenance, provider link,
actual degraded/refused behavior, uncertain external effects and attribution
loss. Record whether backups were only preserved, file-verified, or restored
under a separate approval; include data gap/RPO/RTO only when evidenced. Name
repair and follow-up owners. Do not declare full recovery from `/health` alone.

Source: [`gateway.rs:140–161,302–319`](../crates/bot/src/gateway.rs),
[`gateway_session.rs:61–177`](../crates/cutover/src/gateway_session.rs),
[`website_jobs.rs:69–86`](../crates/bot/src/website_jobs.rs),
[`jobs.rs:90–145`](../crates/bot/src/jobs.rs),
[`server.rs:38–72`](../crates/bot/src/server.rs),
[`metrics.rs:254–291`](../crates/core/src/metrics.rs),
[`Worker routing:52–65,84–90`](../wrangler/src/index.ts),
[`redirect-store.ts:84–107`](../wrangler/src/redirect-store.ts), [backup contract](backup.md).

### Suspected bot-token compromise: containment only

**Detection.** A report of token exposure or unexpected actions by the bot
identity is sufficient to start triage; do not paste the leaked value or test it.
Credential rejection may cause generic
`durable gateway failed; checkpoint unchanged, readiness unavailable`, not a
dedicated compromise signal. `gateway prerequisites missing; gateway parked,
/readyz reports down` (`missing`, `status`) describes a newly parked runtime,
not revocation or stopping an older process. Existing private
`two_bot_rest_requests_total{route,result="4xx"}` can corroborate rejections but
does not distinguish 401/403 or prove compromise; there is **no invalid-token or
compromise counter**. Health/readiness, that metric and reconnect counts cannot
prove a token has not been copied. Preserve sanitized deployment/guild audit
references and available lifecycle evidence, not raw credential-containing logs.
Worker `two-bot container stopped` and container `SIGTERM received; draining`
are useful stop observations, but neither proves absence of revival.

**First five minutes.**

1. Open an incident and record the affected bot application/guild/environment,
   suspected exposure time, source of the report and deployment identity with
   **names/IDs only**. Notify the engineering manager and security responder;
   route owner-reserved credential work through the CEO's consolidated path.
2. Stop agent/operator deploys, manual sends and replay activity using this
   identity. Do not start another process, borrow the legacy/staging token,
   fetch a token to compare it, or try the suspected credential against Discord.
3. Request authorized **Worker-side containment** of singleton `two-bot`:
   suppress probe forwarding/auto-start and all pending/rescheduled keepalive
   calls, then signal the bound Container through the pinned SDK `stop()`.
   This is an operator execution prerequisite, **not a shipped public command**.
   Do not expose an unauthenticated stop endpoint or use `destroy()` to delete
   incident state.
4. Have the **owner/authorized credential custodian**, not an agent, revoke the
   compromised bot token in the [Discord Developer Portal](https://discord.com/developers/applications).
   Discord's Bot-page **Reset Token** invalidates the old token and issues a
   replacement: the entire action is owner-only credential rotation, not an
   agent-executed revocation workaround. Do not copy the old or replacement
   token into a ticket, argument, log, PR, chat or archive.

**Containment boundary / current control gap.** The pinned Container SDK has an
inherited one-shot `stop()` method, but this Worker has **no authenticated HTTP
stop route or persistent incident-pause control**. `onStop` logs only; keepalive
and health/readiness `containerFetch` calls can start the container again.
`KEEPALIVE_SECONDS=0` falls back to 60 seconds; it is not an off switch. Therefore
neither a process kill, a single `stop()`, an audit-library halt nor disabling a
health monitor proves containment. The authorized Worker/platform operator must
provide a verified maintenance/stop execution path that suppresses **both**
start sources before stopping. If unavailable, record containment as **blocked**
and escalate the exact missing control to the engineering manager; do not
invent a CLI/RPC endpoint or implement an unreviewed emergency deployment here.
Owner revocation is still necessary: stopping our container does not stop
someone else using a copied token.

**Containment verification, not restart.** Using authorized platform evidence,
confirm the affected singleton is stopped, pending/rescheduled keepalive and
probe auto-start are suppressed, and no old-identity process is running. Avoid
polling the old Worker `/health` or `/readyz` routes while contained: those probes
can revive the container. Observe logs/platform state over at least two previous
keepalive intervals and record the stop time and no-revival evidence. Obtain the
owner's names-only revocation receipt; do **not** authenticate with the old token
as a test. If either control cannot be verified, containment remains incomplete.

**Recovery handoff / post-incident record.** This playbook ends with verified
containment and custodian handoff, **not automatic restart**. The owner performs
rotation and approved secret provisioning; agents must not create, delete,
rotate, export or install a replacement credential. Restart needs its separate
reviewed deployment, security/access gate, known-good version/image pair and
staging verification. Record exposure/containment/revocation times, non-secret
identity/surfaces, affected actions and uncertain damage, preserved redacted
evidence, actual Worker-stop/no-revival receipt, any missing controls, and the
security/owner recovery handoff. Never interpret silence as rotation approval.

Source: [`Worker lifecycle/routing:104–160,173–175`](../wrangler/src/index.ts),
[`main.rs:151–180,193–199`](../crates/bot/src/main.rs),
[`metrics.rs:239–253`](../crates/core/src/metrics.rs),
[`server.rs:119–121`](../crates/bot/src/server.rs),
[`pinned SDK package`](../wrangler/package-lock.json),
[Container lifecycle hooks and methods](https://developers.cloudflare.com/containers/api/container-class/),
[secret inventory](#secret-inventory-names-only).

## Secret inventory: names only

Never print values, put them in CLI arguments/PRs/incidents, or dump the
container environment. Existing secret provisioning is a separate gated
operation; this inventory is not a request to create, rotate or delete one.

| Surface | Names | Boundary |
|---|---|---|
| Gateway Worker secrets forwarded to container | `DISCORD_TOKEN`, `DATABASE_URL` | Current runtime spellings. `GUILD_ID` is also stored as a Worker secret in staging, but is an identifier, not a credential. |
| Staging deployment CI secrets | `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID` | Existing deploy workflow; no personal credential substitution. `STAGING_WORKER_URL` is a repository **variable**. |
| Backup database connections | `TWO_DATABASE_URL`, `TWO_RESTORE_URL` | Different source/target names; scratch-only in examples. |
| Off-box upload | `TWO_BACKUP_S3_ACCESS_KEY_ID`, `TWO_BACKUP_S3_SECRET_ACCESS_KEY` | Provisioned S3/R2 destination only; see backup.md for non-secret endpoint/bucket settings. |
| Guild-config staging credential | `discord_staging_token` (`CREDENTIALS_DIRECTORY`), `DISCORD_STAGING_BOT_TOKEN` | File first, absent-only explicit fallback. Token/guild pinned to staging identity. |
| Catalogue/core-only credential names | `DISCORD_BOT_TOKEN`, `TWO_STAGING_DATABASE_URL`, `TWO_MODERATION_AUDIT_SECRET`, `TWO_ONBOARDING_ROTA_PSEUDONYM_KEY` | Not aliases consumed by gateway startup; presence in the settings catalogue does not mean runtime wiring. |

## Legacy review and deliberately unported operations

Primary review baseline: legacy `two-bot` **`d5d1179348feb9157bcac8c875de9399d4f5c76a`**,
the frozen source named by [parity.md](parity.md). Compared its
[`RUNBOOK.md`](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/docs/RUNBOOK.md),
[`DEPLOY.md`](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/docs/DEPLOY.md),
[`SECRETS.md`](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/docs/SECRETS.md),
and [`STAGING.md`](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/docs/STAGING.md).
The section mapping below distinguishes replacements, available-but-unwired
ports, intentional drops and cutover-only work. Never copy a legacy npm command
into this Rust binary without an implemented dispatch path.

| Legacy section/surface | two-bot-next disposition and reason |
|---|---|
| Alive/logs/restart, DB health, SIGTERM | Replaced by health/readiness, Worker/container observations and durable restart above. Legacy ports and npm start do not apply; no independent DB health probe. |
| Redeploy/rollback/Coolify deployment | Replaced by explicit Wrangler environment/version/image procedure. Current production cutover remains separate; no Coolify action is prescribed here. |
| Backup/restore, off-box storage, timers | Ported CLI/unit templates; [backup.md](backup.md). Only test-container rehearsals authorized here; no assertion timers are installed. |
| Guild-config snapshot/restore/seal/drift | Ported staging-guarded CLI; not permission to run a real restore or use a live token. |
| Operational audit kill switch/MAC reasons | Core/store port exists; delivery/operator control not wired. No fake SQL/CLI replacement. |
| Automod, moderation, feature toggles | Core policy ports exist; shared sticky/feed command runtime and supervised jobs are now wired, but these moderation action slices and Worker flag forwarding are absent. Documented boundaries above, not operational enforcement. |
| Health/REST rate limits/429 breaker | Liveness/readiness and executor retry contracts documented. Legacy shared global/route breaker is absent, not a library-only port; no reset endpoint. Legacy writer must use its own runbook. |
| Attribution/scorecard/plan/onboarding/automation reports | Legacy read-only reporting CLIs intentionally dropped by parity decision; core calculations/store presence does not add operator commands. |
| Staging provision/verify/reset and Discord e2e harness | Legacy tooling intentionally dropped; next uses local fixtures/agent-testdb and its own soak evidence. Never reset live/staging databases for tests. |
| Preflight/exact-grant acceptance, Wave 0 exports, Wick whitelist, redesign/role cleanup | No equivalent next operator preflight/export/grant CLI identified. Core permission contracts are not live-grant proof; redesign and third-party dashboard verification are outside routine operations. |
| Cutover import/backfill/capture/dedupe, dual-running, command registration, live identity/bot removal | Cutover tools are separate ports, **not dropped**. Execution is out of scope and has separate gates. This runbook cannot authorize activation, backfill, or legacy-bot removal. |
| Host bootstrap/systemd, 48-hour backups, logrotate/housekeeping | Deployment topology differs; timers remain templates. Host execution is broker/operator-owned, not an agent shell task or Container installation recipe. |
| Secret storage/rotation and legacy credential filenames | Names/custody guidance retained, runtime spellings separated. No rotation commands: credential changes require separate approval and exact consumer wiring. |
| Staging acceptance fixtures, hierarchy/grants, bot startup mapping and evidence | Replaced by next test-container/local-fixture coverage and the explicit staging soak. Legacy reset/provisioning evidence and contradictory historical status are not current next-runtime proof. |
| Event timing/voice coverage and deliberate non-behaviors | Open voice durations are not recovered; REST cannot reconstruct gateway voice history. No automated member-contact action is authorized by this operations guide. |

Also checked the later locally cached legacy docs at
[`96777468472f23a02a1e97a43ffab3912fe5df2a`](https://github.com/TogetherWeOwn/two-bot/tree/96777468472f23a02a1e97a43ffab3912fe5df2a/docs)
(2026-09-30), without replacing the frozen parity baseline. Added doctor cadence,
deploy-on-merge smoke gates, broader credential rotations, reporting/leveling/
automation proofs and a full npm index remain legacy-specific: none adds a next
binary command. Their useful constraints are retained here: green exact-head
checks, explicit target, fresh readiness, secret isolation, and distinguishing
offline/help proof from a live success. `STAGING.md` is unchanged between those
sources and contains conflicting historical readiness claims; no old grant,
host address, database derivation or drill is promoted into current authority.

## Keep the runbook honest

```bash
npm --prefix wrangler test
```

`wrangler/test/runbook.test.ts` greps each selected binary dispatch/parser for
supported options, requires operational npm aliases to invoke pinned Wrangler,
and invokes **only command-path `--help`** on the installed pinned Wrangler.
It checks example flags against that command's advertised flag sections because
`--help` alone skips argument validation. Cross-command flags, misspelled options
and malformed-alias fixtures verify refusal.
It runs in `worker check` without credentials or deployed services. It verifies
command existence, **not authorization, successful deployment, backup custody,
Discord effects, or a live rollback**. Required exact-head merge gates remain
`check` (fmt, clippy -D warnings, tests, cargo-deny), `worker check`, `pr-lint`,
and `gitleaks`; a docs test is not a waiver.
