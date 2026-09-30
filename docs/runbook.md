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
| `/readyz` 200, `{"components":[["process","ready"],["gateway","ready"]]}` | READY/RESUMED dispatch has committed. Only these two components are currently wired. |
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
health**. `/metrics`, internal-action endpoints and `/voice/ownership/health`
are not wired bot endpoints.

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

### Sustained-unready alerts

The keepalive records consecutive failed readiness probes and emits one alert
and one recovery per incident. See [Container readiness monitoring](container-readiness.md)
for threshold tuning, the optional Worker-only webhook secret, delivery limits
and response guidance. `container_keepalive_arm_failed` indicates monitoring
setup failed; health/readiness responses still reflect the Container, not proof
that monitoring is armed.

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
- A routine redeploy may auto-apply embedded database migrations at startup.
  This is not permission to perform manual SQL, restore, or migration tests on
  staging/production databases.

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
It connects Postgres (fixed pool maximum 5), applies embedded migrations, loads
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
binary runs the gateway/cache/funnel pipeline. It does not start audit delivery,
automod sanction workers, moderation/slash-command handlers, internal-action
HTTP, or the settings poller. The Worker does not forward the feature vars.
There is no binary audit-halt command or hot-reload/admin endpoint to recommend.

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
| Automations/announcements/text | `TWO_AUTOMATIONS=1`, `TWO_ANNOUNCEMENTS=1`; text needs automations **and** `TWO_TEXT_COMMANDS=1`. | Library gates; no command publishing/job service wired. |
| Onboarding | `TWO_ONBOARDING_MODE=legacy|session|anchor`, default legacy; `TWO_ONBOARDING_DRY_RUN=1`. | Core only; session/anchor roles and dry-run are not active runtime switches. |
| Community scorecard | `TWO_COMMUNITY_SCORECARD=1`; recommendations on unless `TWO_COMMUNITY_RECOMMENDATIONS=0`. | Core/store present; no scorecard scheduler wired. |
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
| DB unreachable / migrations fail | `DATABASE_URL` is required for the configured gateway. Connect/migration/hydration/checkpoint failure exits the process; underlying SQL error is intentionally withheld from runtime logs. Observe the container failure and existing DB-service incident evidence; do not run SQL/probes/tests against staging or production. Fix the named binding/network/schema dependency through its owner, not another credential. |
| HTTP 200 health but persistent 503 ready | Listener works, gateway does not. Read `gateway` state and startup logs. Never soften readiness or count the scaffold-era deploy gate as recovery. |
| Reconnect / RESUME refused | Follow [restart semantics](#restart-semantics-durable-resume-not-full-state-recovery); 4007/4009 force fresh IDENTIFY. Preserve the durable checkpoint, don't hand-edit sequence or start another shard. |
| Discord REST 429 / suspected breaker | The legacy shared global/route 429 breaker is absent; current gateway binary also has no wired REST action executor to reset. The ported executor honors retry-after (body then header, fallback 1 s, +250 ms, cap 60 s). Moderation uses one timed attempt; paced kick/publishing have bounded attempts, but paced GET 429 retries are not count-bounded. Do not claim every REST request has five retries, hammer Discord, replay uncertain moderation writes, or invent a breaker-reset command. Identify the real writer and use its verified containment. See [`executor.rs`](../crates/discord/src/executor.rs). |
| Ready but feature inactive | Gateway readiness says nothing about library-only commands/jobs/kill switches. Check [runtime boundaries](#containment-kill-switches-and-feature-flags), not extra environment guesses. |
| Worker restored but Rust regression remains | Worker-version rollback did not prove image rollback. Inspect the active image and use a schema-compatible full redeploy of the known-good pair. |
| Backup/drill red | Preserve valid archives; inspect exit status, verifier line, table counts and off-box receipt. Rehearse only on a prepared test database. No automatic promotion to a production restore. |

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
| Automod, moderation, feature toggles | Core policy ports exist; runtime command/job execution and env forwarding are absent. Documented boundaries above, not operational enforcement. |
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
