# two-bot-next operations runbook

For the on-call operator of the Rust bot and its Cloudflare Worker/Container.
This describes the shipped source, not proof that an environment is deployed,
that a soak passed, or that production cutover is approved. Cutover, production
restores, token rotation, and live-guild changes need their separate authorization.

## Start here

1. Confirm the affected environment, reviewed Git SHA, Worker version ID,
   container image, time of last good readiness, and incident reference.
2. Read [persisted ownership](#persisted-ownership-control) before a probe that
   could start the gateway. Then check **both** liveness and readiness; capture
   their component breakdown.
3. Read Worker **and container** logs. Contain the problem before redeploying;
   do not turn on an unwired feature in an attempt to repair it.
4. For a regression, select a known-good version/image pair compatible with the
   current database schema. Follow [redeploy/rollback](#redeploy-and-rollback).
5. Record the resulting deployment/version/image and first ready time. A command
   returning success alone does not prove recovery.

All npm commands below run **from the repository root** and use the pinned
Wrangler 4.147.0 through `wrangler/package.json`. Use only the already-authorized
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
| `/readyz` 200; `process`, `gateway`, `database` and `token_invalid` ready | READY/RESUMED dispatch has committed, the bounded live database ping succeeded and no bot-token refusal is latched. The response also includes informational job state; it is not end-to-end feature proof. |
| `/readyz` 503; `gateway` ready but `database` down | Gateway state and database connectivity are separate components. The bounded database ping failed; inspect the affected persistence path without assuming a Discord outage. |
| 503 `{"error":"ownership_fenced","reason":...}` | Worker/DO did not admit the probe. Read ownership state; never interpret this as the container's readiness breakdown or bypass it with a direct start. |
| `/readyz` 503, gateway `down` | Missing gateway prerequisites; service is parked. Check binding **names**, not values. |
| `/readyz` 503, gateway `starting` | Connecting/reconnecting or bounded checkpoint I/O. Compare duration with logs; persistent 503 is not healthy operation. |
| Neither route answers / 500 / HTTP 1101 | Inspect Worker bindings and container startup. Named environments must repeat all Container/DO/exports wiring; do not bypass readiness. |

The same two routes are graded by the read-only cutover acceptance probes
(`scripts/qa_cutover_probes.py`, stdlib only, no credentials, no writes).
Run from the repo root against a local preview or staging by passing its bare
origin as `--base-url` (or `QA_PROBE_BASE_URL`); add `--expect-ready` only when
the gate needs the service itself ready. A parked preview (truthful 503) stays
green without the flag. Each probe line cites the code path it checks. The five
probes cover liveness, the readyz breakdown shape, gateway truthfulness, the
informational jobs map, and that the body is the container's breakdown rather
than an ownership-fence refusal.

The container listens on `LISTEN_ADDR` (default `0.0.0.0:8080`). The Worker
sets it from `BOT_PORT` (default 8080). Both routes use this listener; there is
no legacy health port 9191 or separate voice readiness component. Database
readiness is a bounded live ping on the feature Store pool, not every DB consumer.
For an operator already inside the authorized container, its Docker liveness
probe is also available:

```bash
two-bot --healthcheck
```

It probes local `/health` and exits 0 for HTTP 200, 1 otherwise. This is **not**
a readiness check. The Worker routes these two paths to singleton `two-bot`;
other paths serve the invite redirect, not a bot admin API. The redirect's
`/healthz` can return 200 without starting the bot; **never use it as gateway
health**. The binary exposes private `/metrics`; unauthenticated Worker `/metrics`
is not proxied. An already-authorized responder can use authenticated
`GET /ops/metrics` to proxy the private scrape; that path may start an eligible
container and is not a no-start containment readback. Never expose it publicly.
`POST /internal/actions` is implemented but staging-only and default-dark; both
ingress activation gates, authentication and ownership admission must pass.
It can perform an announcement write when enabled. `/voice/ownership/health`
is not a wired bot endpoint. See the [Internal actions boundary](#containment-kill-switches-and-feature-flags).

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

Workers Logs (dashboard, 7-day retention) keep every staging and production
invocation, unsampled; production traces are off (`wrangler/wrangler.toml`).

`logs` is **Worker/DO tail**, not Rust stdout. Stop it when the bounded incident
observation is complete. For Rust stdout/stderr, use the affected container's
logs in the Cloudflare dashboard. Wrangler 4.147.0 has no `containers logs`
subcommand; do not invent one. Container inspection may list account-wide
resources: match the affected environment/application before taking any action.
No Cloudflare API reaches container stdout either, so voice-event emission has
no CI read path; follow [staging voice-event verification](staging-voice-event-verification.md)
for the dashboard procedure.

Rust uses JSON `tracing` logs, configured by `RUST_LOG`, fallback
`error,two_bot={LOG_LEVEL:-info}` (dependency crates stay ERROR-only unless
`RUST_LOG` opts in); `/readyz` 503s log at DEBUG, not ERROR. This wrapper forwards
`DISCORD_TOKEN`, `DATABASE_URL`, `GUILD_ID`, its computed `LISTEN_ADDR`, the reviewed
`TWO_*` flags (`FORWARDED_FLAGS` in `wrangler/src/container-env.ts`), the validated
`DISCORD_APPLICATION_ID`, and the 12 validated non-secret `DISCORD_*` IDs
(`FORWARDED_DISCORD_IDS` there: audit/voice/moderation log channels, staff alert
channel, ticket category/panel/staff role, landing/goodbye/anchor-welcome channels,
session lobby/looking-to-play) — not `RUST_LOG`, secrets, or arbitrary vars. Adding
a Worker var alone will not configure the container. Do not dump env or HTTP headers
to troubleshoot; redact tokens, connection strings, and member data from evidence.

Look for these literal messages:

- `listening` — HTTP listener bound.
- `gateway prerequisites missing; gateway parked, /readyz reports down` — first
  missing runtime binding named, shard not running.
- `durable gateway initialized; shard connecting` (`resume=true|false`) — startup
  loaded its durable state; not yet proof of a successful RESUMED event.
- `gateway shard loop started` / `gateway reconnect failed; Twilight will retry`.
- `durable gateway failed; checkpoint unchanged, readiness unavailable` — fatal
  initialization failure; underlying SQL error deliberately not logged. Its
  `error_class` is also on `/readyz` as `gateway_failure` for 15 s before exit
  and in Workers Logs as `container_gateway_failure` (see
  [startup-diagnostics.md](startup-diagnostics.md)).
- `container service failed` / `SIGTERM received; draining`.
- Worker: `two-bot container started|stopped`, `two-bot /readyz unhealthy`,
  `two-bot keepalive probe failed`, `two-bot container error`.

The DO renews activity and probes `/readyz` every `KEEPALIVE_SECONDS` (default
60); `sleepAfter` is 30 minutes. Outbound gateway traffic alone does not keep
an idle container awake. Do not disable the keepalive or increase capacity
without measured evidence. `lite`, `max_instances=1` is the declared placement,
not a measurement or B2 acceptance criterion. B2 requires flat memory and has
no accepted numeric RSS threshold; see [staging soak](staging-soak.md).

### Sustained-unready alerts

The keepalive records consecutive failed readiness probes and emits one alert
and one recovery per incident. See [Container readiness monitoring](container-readiness.md)
for threshold tuning, the optional Worker-only webhook secret, delivery limits
and response guidance. `container_keepalive_arm_failed` indicates monitoring
setup failed; health/readiness responses still reflect the Container, not proof
that monitoring is armed.

### Metrics alerts

The Container DO pulls the container-internal `/metrics` on every keepalive tick,
evaluates the rules in `wrangler/src/alert-rules.ts`, and logs/persists each
transition (fire, resolve). It posts to `OPS_ALERT_WEBHOOK_URL` only when
`OPS_ALERT_FORWARDING` is exactly `"on"` (default `"off"`); turning forwarding off
retains the credential and monitoring. See
[metrics](metrics.md#off-container-scrape-and-alert-rules). Fetch the live data
with `curl -H "Authorization: Bearer $METRICS_SCRAPE_TOKEN" "$WORKER_URL/ops/metrics"`.
`METRICS_SCRAPE_TOKEN` must be at least 32 characters; a shorter value leaves
the route at `404` and a short staging token must be reissued (none is
provisioned today). Every scrape attempt takes one token synchronously
before the secret comparison, so concurrent guesses cannot share a token;
an exhausted caller is refused without any comparison (`429` +
`retry-after`). Buckets are per caller, so someone else's failures cannot
throttle a correct bearer elsewhere; a caller shed only because the
10,000-entry table is full is still compared, so a scanner flood cannot
lock out the authenticated scraper.

#### Alert: job stale

A scheduled job's last success is older than two cadences. Check `/readyz` job
status and Worker/container logs for `periodic job failed`. A job that never
succeeded since start (timestamp zero) is not reported here. If the Container
restarted the series resets; wait one cadence before acting. Restart only after
the logs show the job loop is wedged, per the [restart semantics](#restart-semantics-durable-resume-not-full-state-recovery).

#### Alert: job failures

A job failed three completions in a row. Read `periodic job failed` logs (error
class only; payloads are never logged). Usual causes: database unreachable,
Discord REST failing. Fix the dependency; the streak clears on the next success.

#### Alert: REST 429

More than 10% of Discord REST requests between two keepalive samples (minimum
10 requests in the window) returned 429. This is Discord-side rate limiting,
usually a hot route from a recent deploy or a busy job, not proof of a Discord
outage. A counter reset (process restart) skips the window rather than firing.

First response: read the hot route from the `route` label on
`two_bot_rest_requests_total{route,result="429"}` via the authorized
`/ops/metrics` scrape; confirm no deploy is in progress (check the staging
workflow result and recent merges — a fresh deploy can explain a new hot
route). The ported executor already honors `retry-after` per attempt (see
[Common failures](#common-failures)), so do not hammer Discord, replay
uncertain writes, or invent a breaker-reset command. Contain through the
actual writer's verified control
([containment](#containment-kill-switches-and-feature-flags)).

Escalate when the 429 share stays above threshold across several windows after
containment, when the hot route belongs to a writer this team does not own, or
when 429s coincide with 5xx/transport failures suggesting a wider Discord or
network incident.

#### Alert: DB pool

The SQLx pool sat at its maximum with zero idle connections for three
consecutive keepalive samples. This is pool exhaustion, a proxy for DB trouble;
there is no DB error counter yet. It means every checkout is held — new queries
wait rather than fail fast — not proof that Neon itself is down (pool gauges
sample SQLx bookkeeping, not DB reachability).

First response: check Neon status for the staging branch before touching the
bot; then look at recent deploys for a change that could hold checkouts open
(new query path, widened job fan-out, a job whose cadence no longer matches its
duration). Compare against the scrape window — a short burst that self-clears
across the next samples is not exhaustion. Do not run SQL probes against
staging or production, add grants, or restart the container to "free" the
pool; a replacement restarts the shard without fixing a leak.

Escalate when the streak persists after the suspect deploy is identified,
when exhaustion coincides with gateway `starting`/`down` or job-failure
alerts, or when the Neon dashboard shows trouble on the staging branch — the
fix then belongs to the dependency owner, not a redeploy.

#### Alert: DB errors

Three or more storage-layer failures arrived between two keepalive samples
(`two_bot_db_errors_total`, currently counting send-admission SQL; other
stores adopt the counter incrementally). Unlike the pool rule above, this is
errors, not pressure: queries already failed, they are not merely waiting.
A counter reset (process restart) skips the window rather than firing, and a
slow trickle below threshold stays silent — sustained low-rate failures
surface instead through `job_consecutive_failures`.

First response: check Neon status for the staging branch before touching the
bot; then correlate with the `op` label and recent deploys (a new query path
or migration can explain a fresh error burst). Do not run SQL probes against
staging or production, add grants, or restart the container to "clear" the
errors; a replacement restarts the shard without fixing the failing writes.

Escalate when the burst repeats across windows, when it coincides with pool
saturation or job-failure alerts, or when the Neon dashboard shows trouble on
the staging branch — the fix then belongs to the dependency owner.

#### Alert: send admission blocked

Discord sends were refused send-admission in three consecutive keepalive
windows (`two_bot_send_admissions_total{outcome="blocked"}`). Admission is
the token-wide lane in front of every Discord send: a held lane or an active
cooldown refuses new sends rather than queueing them. One busy tick with a
refusal is normal contention and stays silent; only windows with *new*
refusals extend the streak, so an idle bot or a self-clearing burst never
pages. Storage failures of the admission SQL itself count in
`two_bot_db_errors_total`, not here, so one outage pages once via that rule.

First response: read the recent sends from the container logs (rate-limit
cooldowns, held lanes after failed completions) and confirm no deploy is in
progress — a fresh deploy can briefly contend the lane. Do not replay
uncertain writes, hammer Discord, or restart the container to "free" the
lane; a replacement leaves the durable lane row occupied.

Escalate when refusals persist after the suspect deploy or cooldown is
identified, when they coincide with 429 or DB-error alerts, or when sends
stay refused with no cooldown in the logs — the lane may be stuck and the
fix belongs to the on-call engineer, not another redeploy.

#### Alert: voice failures

Voice room lifecycle failures between two keepalive samples
(`two_bot_voice_operations_total`, `two_bot_voice_dead_letters_total`,
`two_bot_voice_orphans_total`). Fires when failed operations exceed 5% of
room operations with at least 10 operations in the window (the T3 room
create/move/delete budget in
[voice-cutover-rollback-triggers.md](voice-cutover-rollback-triggers.md)),
or when any new dead-letter (a queue write that exhausted 10 attempts) or
orphan (an untracked creator-channel orphan needing manual deletion)
appears — so a low-volume stranded-member failure still pages. Any outcome
other than `success` (`category_full`, `discord`, `persistence`,
`cancelled`) counts as a failure: the join did not place a room, or the
move/delete did not complete. A counter reset (process restart) skips the
window rather than firing, and a slow trickle below threshold stays silent.

First response: scope the failing operation from the `op`/`outcome` labels
on `two_bot_voice_operations_total` via the authorized `/ops/metrics`
scrape; confirm no deploy is in progress; then read the container logs for
the matching `voice_event="voice_operation"` warn lines (see the lifecycle
signal inventory in
[voice-lifecycle-alert-drill.md](voice-lifecycle-alert-drill.md)). Do not
retry uncertain room writes, replay member moves, or delete untracked
channels by hand — untracked-channel deletion by the bot is never allowed.

Escalate when failures persist across windows after the suspect deploy is
identified, when a dead-letter or orphan names a stranded member, or when
failures coincide with 429, DB-error or pool alerts — the fix then belongs
to the on-call engineer, not another redeploy.

#### Alert: gateway missed events

`two_bot_gateway_missed_events_total` increased between two keepalive
samples. These are dispatches Discord assigned but this process never
received (sequence gaps inside one session). Any increase fails the
zero-missed-events acceptance: the session continued, but part of the
event stream is gone and RESUME does not replay it. The first sample after
monitoring arms only stores the baseline and never fires, and a counter
reset (process restart) skips the window rather than firing.

First response: read the paired `two_bot_gateway_disconnects_total` counter
via the authorized `/ops/metrics` scrape — a missed-events increase with no
disconnect means the gap predates this instrumentation or the process
restarted mid-window (re-baseline both scrapes after it); a rise next to
disconnects means transport loss with sequence gaps. Correlate with recent
deploys (a fresh deploy restarts the process and resets the counter) and
the container logs for `gateway reconnect failed; Twilight will retry` and
`gateway ready; checkpoint committed`. Do not restart the container to
"clear" the counter; a replacement resets the baseline without recovering
the missed dispatches.

Escalate when the increase repeats across windows, when it coincides with
unpaired disconnects (no later RESUME or fresh READY), or when missed
events rise with no disconnect at all — the gap is then unexplained and
the fix belongs to the on-call engineer, not another redeploy.

#### Alert: ticker stale

A 15 s ticker (`scheduled_messages` or `settings`) recorded no successful
completion for more than 10 minutes
(`two_bot_job_last_success_timestamp_seconds{job}`). These tickers wedge
silently: skipped busy deadlines count neither as success nor failure, so
neither `job_stale` nor `job_consecutive_failures` can see them. A job
that never succeeded since start (timestamp zero) is not reported here:
that covers both boot and parked tickers (never registered because
`DATABASE_URL` is unset or the automations gate is off). If the Container
restarted the series resets; wait one window before acting.

First response: check the `jobs` map on `/readyz` for the ticker's
`parked`, `last_success` and `consecutive_failures` fields, then read the
Worker/container logs for `periodic job failed`. A parked ticker with a
zero timestamp is configuration, not a wedge — confirm the expected
`DATABASE_URL` binding and automations gating before touching the bot.
Restart only after the logs show the ticker loop is wedged, per the
[restart semantics](#restart-semantics-durable-resume-not-full-state-recovery).

Escalate when staleness persists after the suspect deploy or dependency is
identified, when it coincides with pool-saturation or DB-error alerts, or
when a due schedule row or settings change stays unapplied past the
window — the fix then belongs to the on-call engineer, not another
redeploy.

## Persisted ownership control

The Worker/DO fence is implemented, not implicitly released by deployment.
`CF_VERSION_METADATA.id` identifies the eligible Worker version; the singleton's
persisted `deploymentId + epoch + phase` grants ownership. Missing/unreadable
storage fails closed. Startup/proxy/keepalive gates cover the pinned SDK's
separate auto-start paths. A changed version stops an inactive reattached
process and cannot auto-claim the record. The fence coordinates **only one shared
DO namespace/singleton**, not legacy or another namespace. See the full
[control protocol and staging rehearsal](cutover.md#workerdo-ownership-fence-implemented-rehearsal-still-required).

The staging-only client validates a `two-bot-next-staging.<subdomain>.workers.dev`
HTTPS origin, refuses redirects, reads the dedicated `OWNERSHIP_CONTROL_TOKEN`
from an **already approved** environment binding, and never prints it. The same
secret must be provisioned as the staging Worker secret `OWNERSHIP_CONTROL_TOKEN`
and GitHub Actions secret `STAGING_OWNERSHIP_CONTROL_TOKEN` before rollout. This
runbook does not authorize creating/rotating credentials. If binding/provisioning
is missing, route through the Director of Engineering; do not substitute Discord,
QA, Access, production or broad Cloudflare API credentials.

```bash
# Read-only control state; does not start the container.
node wrangler/scripts/ownership-control.mjs status
# The epoch below is the exact authenticated readback, not a guessed default.
# OWNERSHIP_ACTOR must be a non-secret operator/run audit label from the receipt.
node wrangler/scripts/ownership-control.mjs takeover "${CURRENT_OWNER_EPOCH}"
# Release does not start the container: only now may the approved health probe
# start it. Retain /readyz, version, image and single-session observations.
# To park ALL Worker versions before legacy ownership/rollback:
node wrangler/scripts/ownership-control.mjs fence "${CURRENT_OWNER_EPOCH}"
node wrangler/scripts/ownership-control.mjs status
```

`STAGING_WORKER_URL`, `OWNERSHIP_ACTOR` and the approved secret binding are inputs.
Refresh the epoch before **each** change. `fence` is a persisted parking owner
(`deploymentId=null`, `phase=fenced`); health/readyz and stale schedules refuse.
Takeover/fence increment the epoch and record actor, timestamp, old/new epoch and
owner, except a same-version repeat takeover by the deployment that already owns
the active singleton: the Worker returns the stored record unchanged (no write,
no audit row, no teardown) and the client stops with "Ownership transition not
confirmed; preserve maintenance". That is the safe direction; the normal staging
deploy path mints a new version id, so the verify gate is unaffected.
Durable revocation is written before awaited native destruction;
`running=false` is required before active release. A crash, storage-write failure
or unconfirmed shutdown leaves denial; do not assume a 503 stopped the old
process. Preserve maintenance until teardown is confirmed. 401/auth failure is
a stop, 409 requires state reconciliation, and 503 is never permission to clear
storage/alarms. No operation clears SDK state or changes guild/database bindings.
The deployment-takeover client re-reads the fresh epoch on every retry, so reads
answered by converging versions never block the post. The transfer step pins the
client to the receipt-validated Worker version (`OWNERSHIP_EXPECTED_DEPLOYMENT`).
When its posted epoch shows up owned by a deployment other than the one
answering the read, the client re-posts only if the answering version is the
deployed one (old-to-new handover); an answer from any other version is stale,
so it stops without posting rather than handing that version a further commit.
Without the pin the client fails closed and always stops on such a mismatch.

The workflow preflight stops **before deploy** if control configuration is absent.
After deployment it explicitly transfers only a previously active owner. First
boot or an intentionally parked singleton needs an authorized staging manual
dispatch with `release_fence=true`; a routine push never unparks it. It still must
prove readiness, not accept the ownership receipt as a gateway-ready event.
Production control uses B4's separately reviewed execution sheet/API contract;
this client rejects production origins. Never roll back to an unfenced wrapper.

## Redeploy and rollback

### Before changing anything

- Use a clean checkout of the approved revision; tests and exact-head CI must
  be green. No second bot instance/shard for the same guild.
- Record current deployment and known-good **Worker version + Git SHA + image**.
  Versions are not Git SHAs. Check whether migrations or resource changes make
  the old code incompatible; rollback does not rewind Postgres or DO storage.
- Check the current staging workflow result. `deploy-staging.yml` runs on merge
  to `main` unless the merge touches only docs, root markdown or repository
  chrome (then no run starts and staging keeps serving the previous runtime
  commit), and also allows manual dispatch, but currently accepts a 503
  readiness response as a scaffold-era gate. **Workflow green is not gateway
  ready**: require your own first `/readyz` 200 observation and feature evidence.
- Gateway startup connects DML-only and does not migrate; lazy jobs also use
  `skip_migrations=true` and install no web-contract DDL. Schema and grants must
  already be provisioned through the reviewed operator path before runtime
  startup. Missing schema/grants can leave jobs failing `database` while the
  gateway component is ready; a redeploy will not provision them. Confirm schema
  compatibility with its owner. This does not authorize extra grants, manual
  SQL, restores, migrations or migration tests on staging/production.

```bash
npm --prefix wrangler run deployments -- list --env staging
npm --prefix wrangler run versions -- list --env staging
npm --prefix wrangler run versions -- view "${VERSION_ID}" --env staging
```

Wrangler lists the ten most recent versions/deployments. Select the actual
previously healthy version from your deployment record; never silently choose
"latest" or omit the rollback ID.

### Staging-only migration runner

`.github/workflows/staging-migrate.yml` (manual, staging-only, no production
path) runs `staging-migrate --plan|--apply` from `crates/cutover/src/bin/staging_migrate.rs`.
It embeds this crate's migrations through the SQLx **0.9.0 library** (no
`sqlx-cli`; the pin is asserted against `Cargo.lock`), keeps the ledger in
`public._sqlx_migrations`, and runs `SET ROLE` in SQLx's per-connection
`after_connect`, verifying `current_user` on every connection. The two modes
use two credentials and two groups, so the plan is physically read-only:
`--plan` reads only `TWO_BOT_STAGING_PLAN_DATABASE_URL` (a login holding only
`two_bot_migrator_ro`, the `staging-migrate-plan` environment secret) and runs
as `two_bot_migrator_ro`; `--apply` reads only
`TWO_BOT_STAGING_MIGRATOR_DATABASE_URL` (the reviewed `staging-migrate-apply`
environment secret) and runs as `two_bot_migrator`. A mode never reads the
other mode's binding, and an absent binding refuses before any connection.
Invocation (secret-free; each URL comes only from its existing binding):

```text
staging-migrate --plan --source-sha <40hex> --staging-host <host> \
  --staging-database <db> --recovery-evidence-ref <ref> --acl-plan-ref <ref> \
  [--expected-pending <ascending,comma-separated versions>]
staging-migrate --apply <same flags> --expected-pending <list> \
  --plan-manifest-sha256 <64hex> --plan-run-id <run id> \
  --plan-manifest-path <producing run's downloaded manifest>
```

Reconcile is set-based: pending is every source version absent from the
ledger, in source order, so a ledger may lag the source by any subset. `--plan`
prints that list in the manifest (`pending_before`) and changes nothing.
`--apply` requires `--expected-pending` (the workflow input of the same name)
and refuses before any DDL unless it equals the computed pending list exactly,
so apply can only run the pending set a reviewed plan already showed. `--apply`
additionally requires `--plan-manifest-sha256` and `--plan-run-id`: the SHA-256
of the reviewed plan run's uploaded `staging-migrate-manifest.json` and the run
that produced it, plus `--plan-manifest-path`: the producing run's
downloaded `staging-migrate-manifest.json`, which the workflow fetches back
from the `plan_run_id` run before the runner starts. The runner recomputes
the hash over its own source SHA, pending list and full source migration
table (so same-pending-different-SQL replays refuse) and refuses on any
mismatch, binding apply to the exact manifest the reviewed plan produced on
the same `source_sha`. It then parses the downloaded manifest and requires
its embedded `plan_manifest_sha256` to equal the recomputed hash: the
recomputation alone proves exactness from public inputs, while this second
comparison proves the bound hash actually came from the named producing run
(a wrong run id, an expired or missing artifact, or an unreadable,
unparseable or field-less file all refuse). `--plan` prints its own hash
(`plan_manifest_sha256`) in the manifest and ignores the binding flags.
A passing apply records `"plan_provenance_verified": true` in its manifest.

`--plan` also fills an `audit` block (`null` for apply) from one explicit
`READ ONLY` transaction on the same read-only login: the ledger owner
(`ledger_owner`), `ledger_counts` (successful rows, max successful version,
failed rows), one `memberships` entry per migrator login (`two_bot_migrator`,
`two_bot_migrator_ro`, `two_bot_migrator_ro_plan`, `two_bot_migrator_apply`):
an `exists` flag (so a missing login reads `false` instead of vanishing), the
aggregated direct `member_of` list, and the transitive `member_of_migrator` /
`member_of_ro` flags that mirror the plan login guard, plus
`verify_findings`, the output of `sql/verify_database_roles.sql` rendered with
`sql/database_role_matrix.sql` (empty means no drift). The files are the ones
compiled from the dispatched `source_sha`, and their SHA-256 digests
(`matrix_sha256`, `verify_sha256`) are echoed so a reviewer can compare them
with another SHA. The block holds names, counts and findings only, sits
outside `plan_manifest_sha256`, and a failed readout reports a fixed `error`
string instead of failing the plan.

It refuses (exit 2, before any DDL) when the binding is absent, the target does
not equal the pinned staging host/database inputs, either pin is empty or looks
like production, either host pin or the binding host is a pooler endpoint
(session `SET ROLE` and the migrator lock need the direct endpoint), the login
cannot assume `two_bot_migrator` (apply) or `two_bot_migrator_ro` (plan), the
plan login also holds `two_bot_migrator`, a reference is missing, `--apply` has no
`--expected-pending` or it mismatches, `--apply` has no `plan_manifest_sha256`/
`plan_run_id` or the hash does not match the recomputed manifest, `--apply`
has no producing-run manifest or that manifest does not carry the bound hash,
or the ledger has a failed/incomplete
row, a SHA-384 mismatch or a version unknown to the source. The database name needs no `staging`
substring (the verified shared-Neon staging database is `two_bot`); the pinned
host plus the binding-match check is the staging identity. It never resets,
reverts, restores, creates roles or grants. The sanitized JSON manifest (source
SHA, per-migration SHA-384, ledger before/after, applied count, plus its own
`plan_manifest_sha256` and the bound `plan_run_id`) is the evidence;
on failure the ledger-after is preserved, not repaired.

The workflow runs only when dispatched from `main` and splits into three jobs.
The `plan` job always runs and reads the binding from the `staging-migrate-plan`
GitHub environment, which carries no reviewer because planning changes nothing;
it uploads `staging-migrate-manifest.json` as the `staging-migrate-manifest`
run artifact (14-day retention), which is where the reviewer reads
`plan_manifest_sha256`/`plan_run_id` for the apply dispatch. The digest is the
embedded source/pending/migration projection, **not** SHA-256 of the JSON or ZIP.
For `mode: apply`, the unprotected `claim` job validates this dispatch's read-only
plan against the request and uploads `staging-migrate-apply-claim.json` as the
`staging-migrate-apply-claim` artifact, before apply waits for environment approval.
It has no database secrets or environment; a step inside the waiting apply job
cannot publish evidence before approval. Both artifact uploads disable compression
for the bounded stored-ZIP reader. The [claim contract](staging-migrate-claim.md)
defines the exact versioned fields, serialization vector and consumer requirements.
The claim is a request, not proof of producer provenance or CEO GO; the independent
protection-rule consumer must authenticate both runs and the prior plan artifact.
The `apply` job runs only for `mode: apply`, after a green plan **and claim**, and
reads the binding from the `staging-migrate-apply` environment, which must have a
required reviewer and a main-only deployment-branch rule. Both bindings must be
environment secrets, not repository secrets; otherwise a workflow edited on
another branch could read them. This change does not create the environments or
the secrets: create both before dispatch, or the jobs fail instead of running.

Prerequisites the legitimate principal must verify **before dispatch** (the
runner cannot, and this change does not claim them): the real staging Neon
identity; that both dedicated bindings already exist, each in its own
environment: `TWO_BOT_STAGING_PLAN_DATABASE_URL` (a login holding only
`two_bot_migrator_ro`) in `staging-migrate-plan` for the `plan` job, and
`TWO_BOT_STAGING_MIGRATOR_DATABASE_URL` in `staging-migrate-apply` for the
`apply` job, since the two jobs never share a credential; the
`staging-migrate-plan` / `staging-migrate-apply` environment protections
above; and a complete
recovery set covering the Next schema, `_sqlx_migrations` ledger, object
ownership, ACLs and logins. The generic legacy backup omits Next tables and the
SQLx history, and unverified Neon PITR is not a working recovery. Apply the
reviewed ACL sequence in `docs/database-roles.md` so other shared-database
services keep their access. Real SQLx proof runs only against disposable CI
services (`crates/cutover/tests/staging_migrate_db.rs`).

The plan login must hold only `two_bot_migrator_ro`: `--plan` refuses, before it
reads the ledger, when the login is a member of `two_bot_migrator` (directly,
by inheritance, or as a superuser), and the refusal names the role and never
the login or the URL. The `source_sha` input picks the commit whose runner is
built, while the plan/apply split itself comes from the workflow on `main`. A
plan dispatched with a `source_sha` older than `3d1e2ddd` therefore builds the
pre-split runner, which looks for `TWO_BOT_STAGING_MIGRATOR_DATABASE_URL`; the
`plan` job never exports that binding, so the old runner refuses before any
connection (fail closed). Dispatch plan and apply with a `source_sha` at or
after the split.

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
both Worker version and running image. Unlike CI's explicit handoff step, the
bare deploy command does **not** transfer ownership: read control state, confirm
old-process teardown, and perform the authorized takeover with its current epoch
before any startup-capable probe. Then check `/health`, `/readyz` and startup
logs; record finish-to-first-ready as a separate deployment/workflow interval.
It is not the B2 outage-start-to-verified-recovery measure. A container
replacement can restart the shard; there is no promise of zero downtime.

### Worker-version rollback

For a Worker-only regression with compatible current image/resources:

```bash
npm --prefix wrangler run rollback -- "${VERSION_ID}" --env staging
npm --prefix wrangler run deployments -- list --env staging
```

Read the interactive target and confirm the incident's known-good version.
Do not add `--yes` or override warnings. Wrangler 4.147 updates Durable Object
code with `deferred` mode and a 300 s maximum delay by default, so a bare
rollback can leave the old code serving for up to five minutes. For a manual
incident rollback add `--durable-objects-code-update-mode immediate`, and expect
a time-to-ready that includes the restart. If Wrangler reports changed secrets,
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

Before rollback, persist a parking fence and confirm native destruction. Roll
back **only to another fence-capable version**; an unfenced baseline ignores this
record and could restart an unauthorized gateway. After either recovery, read
ownership, explicitly take over the current epoch under the incident's handoff
authorization, then repeat health/readiness/log observations. Confirm only one
gateway session and record version, image, first ready time and remaining
limitations. None of these examples were a live rollback drill.

**Staging drill.** The manual `staging-rollback-drill` workflow (dispatch from
`main`, input `target_version`) runs this procedure end to end in the protected
`staging` environment: fence, one unforced Cloudflare deployment of the target
with `code_update_strategy: immediate`, epoch-checked takeover, a 2 s
`/readyz` + `/health` poll, then the same sequence back to the original version.
Pick the target from the deployment list: a version that already served staging
traffic, not the serving one, compatible with the current schema (the script
refuses an unknown or never-deployed id). The job summary and evidence file hold
the fence, rollback, takeover and first-ready times, the time-to-ready against
the workflow's 60 s budget, probe counts, the container image digest and
instance counts. This workflow interval is not an outage-start recovery
measurement and cannot prove B2's under-60-second recovery criterion.
Gateway-session count is not observable from probes; read the Worker logs for it.
If the run stops on a 401/403 it skips the restore: recover with a
`deploy-staging` dispatch with `release_fence=true` after the binding is fixed.

**Container-image backout/restore drill.** The manual `staging-container-drill`
workflow (dispatch from `main` only) backouts staging to a previously reviewed
Rust source/image pair and restores the baseline through the FULL container
rollout path. It is the answer to the Worker-only drill's gap (same image on
both legs, no session witness): every leg fences the singleton, runs a full
`wrangler deploy` of the pinned pre-built image, takes over with an
epoch-checked handoff, and verifies a converged `full_auto` rollout plus the
exact Worker version, source revision, build id and running image digest
before timing first ready. Old source is never checked out and no Dockerfile
is rebuilt: the deploy config is generated from the reviewed `wrangler.toml`
on `main` with only the container image overridden to the digest-pinned
registry reference.

Dispatch contract (all pins immutable; `latest` and mutable tags are refused):
`backout_source_sha` (40-hex), `backout_build_id`, `backout_image`
(`registry...@sha256:...`), `backout_worker_version` (UUID),
`backout_rollout_id` (prior completed staging rollout that served the image),
`backout_review_ref` (format `PR-<n>:ci-ok-<shortsha>`, strict tokens only),
`backout_staging_run_id` (prior successful staging deployment run),
`compatibility_note` (format `schema-<id>+DO-<state>+flags-<state>`, strict
tokens only), and optional `session_attestation` (counts and windows only;
see below). Before dispatch, recheck the pair with read-only calls: the
source commit is on `main` with green required checks and an independent
review; the staging run's evidence shows the same digest serving; the schema,
Durable Object state, bindings and enabled flags are unchanged since that
pair (no DB or storage rewind and no migrations happen in the drill, by
construction). The script cross-checks live what it can (prior rollout
completed with the image, Worker version served traffic, same namespace
binding, baseline healthy with a proven revision) and refuses same-image,
never-served and incompatible pairs before any change.

Session witness: one healthy instance or `/readyz` is not acceptance. The
script records a temporally complete probe timeline per leg and requires a
covering operator log-count attestation of exactly one distinct gateway
session across each handoff window (`fenced` to `first_ready` in the
evidence). Obtain the counts from the staging Worker logs over those UTC
windows and record only the counts, never session identifiers, tokens,
bodies or member content. Without that attestation the witness is
NOT_PROVEN and the drill fails acceptance while still restoring the
baseline; mark missing coverage NOT_PROVEN, never PASS. Same-image evidence
is rejected as container-drill acceptance. On a 401/403 the restore is
skipped with no credential fallback: recover with a `deploy-staging`
dispatch with `release_fence=true` after the binding is fixed.

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

### Force-fresh IDENTIFY (first production boot only)

First production boot only: the age policy above alone would RESUME a
checkpoint up to 15 minutes old, so the first production boot arms a one-shot
directive to force a fresh IDENTIFY instead. Authority and full contract:
[Force-fresh IDENTIFY](gateway-recovery.md#force-fresh-identify-first-production-boot).
`TWO_DATABASE_URL` is the bot database the target Container uses; `GUILD_ID`
is that Container's configured guild, and `--guild` must equal it. Quoted
verbatim from the authority (only the `two-bot` binary ships in the image;
run from an operator checkout):

```sh
# 1. Dry run (default): prints guild, shard 0, checkpoint age and directive; writes nothing.
cargo run -p two-bot-cutover --bin gateway-force-identify --locked -- --guild "$GUILD_ID"
# 2. Arm. The live guild also needs --allow-live-guild.
cargo run -p two-bot-cutover --bin gateway-force-identify --locked -- \
  --guild "$GUILD_ID" --apply --reason "first production boot" --allow-live-guild
# 3. Start the bot, then re-run the dry run: the directive shows "consumed at ...".
```

The live (production) guild refuses without `--allow-live-guild`. One-shot
consume semantics: of two concurrent boot reads, exactly one consumes the
directive; the directive never deletes or rewrites `gateway_sessions` — the
bot's existing discard path clears the checkpoint before IDENTIFY, and the
next boot after READY has no directive and RESUMEs normally.

## Containment, kill switches and feature flags

**Do not confuse a feature gate with a bot-wide stop control.** Moderation and
automod are implemented and gated: the binary registers channel-moderation
handlers and composes automod into gateway processing. The Worker explicitly
forwards `TWO_MODERATION`, `TWO_AUTOMOD` and `TWO_AUTOMOD_ENFORCE`. They can perform
writes on a permitted, configured identity; their existence does not authorize
activation. Current activation admits these capabilities only for the staging
guild/application pair; live flags alone cannot enable them. Registry entries
do not prove every builtin has a handler. Internal-action HTTP is the
dark-by-default private announcement receiver (see the Internal actions row).
There is no binary audit-halt command or settings hot-reload/admin endpoint to
recommend. See the [incident playbooks](#incident-playbooks) for persistent
ownership containment, actual degradation and stop boundaries.

For unexpected writes: first identify the actual writer (legacy bot, next image,
other automation). Preserve evidence and use the authorized deployment/containment
procedure for that writer. A legacy flag or a SQL row is **not** a verified
next-runtime kill switch. Do not issue SQL against live/staging services, switch
tokens, or redeploy with an unreviewed wiring change during this docs procedure.

| Control | Ported contract | Current operational boundary |
|---|---|---|
| Audit kill switch | Durable `audit_kill_switch`, singleton `id=1`; `engage_halt(actor_id)` / `disengage_halt()` library methods. Halt blocks queue discovery/claim/preparation, not fact recording or an already prepared/in-flight send. | Store exists, delivery service/operator endpoint not wired. No supported command in this binary. Durable DB errors propagate; legacy pure-model read-fail-open is **not** send authorization. See [audit store](audit-store.md). |
| Automod | Exact `TWO_AUTOMOD=1` enables config. Dry-run by default; exact `TWO_AUTOMOD_ENFORCE=1` selects enforce. | Forwarded and composed into the gateway when activation/configuration permit. Requires valid Owen/protected-role configuration; enforcement can sanction. `TWO_AUTOMOD=1` requests MESSAGE_CONTENT intent. Flags alone do not override identity/permission gates or stop the bot. |
| Moderation | Exact `TWO_MODERATION=1`; enabled config requires `TWO_OWEN_USER_ID`; protected-role policy applies. | Forwarded; channel handlers `/purge`, `/slowmode`, `/lockdown`, `/unlock` are registered and subject to activation and permission checks. Do not infer every member-moderation builtin executes. No config toggle demonstrated as an emergency stop. |
| Automations/announcements/text | `TWO_AUTOMATIONS=1`, `TWO_ANNOUNCEMENTS=1`; text needs automations **and** `TWO_TEXT_COMMANDS=1`. | Per-feature gates, not operational containment. Verify the affected deployed handler; registry publishing or a periodic job alone does not prove a specific action is active. |
| Onboarding | `TWO_ONBOARDING_MODE=legacy|session|anchor`, default legacy; `TWO_ONBOARDING_DRY_RUN=1`. | Mode/dry-run contracts are feature-scoped, not bot-wide stop controls. Verify the affected handler and staging evidence; a catalogue value alone is not a runtime activation or reload receipt. |
| Community scorecard | `TWO_COMMUNITY_SCORECARD=1`; recommendations on unless `TWO_COMMUNITY_RECOMMENDATIONS=0`. | Conditionally registered supervised job; durable retry budget and completion rules apply. A successful/no-op tick is not fresh publication proof. See the [database playbook](#neon-or-hyperdrive-outage). |
| Internal actions | `announcement.post`, `role.assign` and `event.upsert` are on whenever the receiver is; `event.cancel` needs `TWO_INTERNAL_ALLOW_EVENT_CANCEL=1`, `event.read` needs `TWO_INTERNAL_ALLOW_EVENT_READ=1`, `settings.get`/`settings.set` need `TWO_INTERNAL_ALLOW_SETTINGS=1`, `guild.add_member` needs `TWO_INTERNAL_ALLOW_ADD_MEMBER=1`, member and channel moderation need `TWO_INTERNAL_ALLOW_MODERATION=1` **and** `TWO_MODERATION=1`; remaining verbs stay refused. | Wired receivers are `announcement.post`, `role.assign` and `event.upsert` (no extra flag: contained only by the dark switch below), `event.cancel`, `event.read`, `settings.get`, `settings.set`, `guild.add_member`, `moderation.ban`, `moderation.tempban`, `moderation.kick`, `moderation.warn`, `moderation.timeout`, `moderation.purge`, `moderation.slowmode`, `moderation.lockdown`, `moderation.unlock`, staging only: dark until the Operator sets the Worker secret `TWO_INTERNAL_ACTIONS` to `1` last; unset it to go dark again. Reachable solely through the staging Worker ingress for `POST /internal/actions`; production has none. Unsetting an allow flag stops that verb. See [staging ingress](internal-actions-receiver.md#staging-ingress-default-dark) and the [receiver verb list](internal-actions-receiver.md). |
| Settings hot reload | Typed catalogue/store with env-only secret/moderation keys. | Poller/runtime rebuilding remains follow-up; no promise of changes applying without restart. |

Source: [`automod.rs`](../crates/core/src/automod.rs),
[`moderation.rs`](../crates/core/src/moderation.rs),
[`feature_commands.rs`](../crates/core/src/feature_commands.rs),
[`onboarding.rs`](../crates/core/src/onboarding.rs),
[`settings.rs`](../crates/core/src/settings.rs),
[`pipeline.rs`](../crates/discord/src/pipeline.rs),
[`container-env.ts` forwarding](../wrangler/src/container-env.ts),
[`activation.rs` identity narrowing](../crates/core/src/activation.rs),
[`command_runtime.rs` channel dispatch](../crates/bot/src/command_runtime.rs),
[`main.rs` automod composition](../crates/bot/src/main.rs),
[`automod_gateway.rs`](../crates/bot/src/automod_gateway.rs).
Do not enable new writes until runtime wiring, environment propagation,
containment, authorization and staging evidence exist.

### Automation definition quotas

Ordinary store-backed creation permits **25 schedules**, **25 feed relays** and
**20 open LFG posts per guild**. Disabled feeds and disabled/completed schedules
still count toward their definition limits. Remove an unused definition with
`/feed-remove` or `/schedule-remove`; close an open post with `/lfg-close`.
The command replies name the exhausted limit and the command that frees a slot.
No Discord post is attempted for a quota-refused LFG creation.

Capacity checks and writes share a short transaction-scoped guild/resource lock.
Schedule replacements and open LFG updates do not consume another slot;
reopening a closed LFG post does. Feed creation remains insert-only. Existing
rows above a limit are preserved, and existing schedule/LFG definitions can
still be updated. These are application CRUD limits, not schema constraints:
operator backup/restore preserves historical rows and is not quota-truncated.

This change does not add retention, trigger/signup cooldowns or feed-poll
fairness. Audit replay markers and uncertain delivery state must remain durable
when implementing those separately. No automatic purge or live-data cleanup is
authorized by these limits.

Source: [`automation_quota.rs`](../crates/core/src/automation_quota.rs),
[`scheduled_store.rs`](../crates/core/src/scheduled_store.rs),
[`feeds_store.rs`](../crates/core/src/feeds_store.rs),
[`lfg_store.rs`](../crates/core/src/lfg_store.rs).

## Backup, restore and drill commands

[Backup procedures and formats](backup.md) are the detailed contract. These
commands are **rehearsals on agent-testdb/local fixtures only**; never point
tests, probes, dry runs with a DB URL, or restore verification at production
or staging databases. No production restore is authorized here.

For a prepared, separately named test database carrying the required schema,
with `TWO_BACKUP_DIR`, retention and an approved upload wrapper already configured.
The drill example also requires a nonempty harness-provided `PAPERCLIP_RUN_SCRATCH_DIR`
and its pre-provisioned, protected `restore-drills` child (mode `0700`); do not
invent a root or run it with an unset scratch binding:

```bash
TWO_DATABASE_URL=postgres://agent_test@agent-testdb:5432/two_next_backup_test two-bot backup
two-bot backup-upload "${BACKUP_FILE}"
env -u TWO_RESTORE_URL two-bot restore "${BACKUP_FILE}" --dry-run
TWO_RESTORE_DRILL_BOOTSTRAP_URL=postgres://agent_test:@agent-testdb:5432/postgres TWO_RESTORE_DRILL_EVIDENCE_DIR="${PAPERCLIP_RUN_SCRATCH_DIR}/restore-drills" two-bot restore-drill "${BACKUP_FILE}" --confirm-scratch
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
authority to use a non-test target. Direct `restore` **does not migrate** and
requires a fresh prepared target; moderation history refuses before truncation.
The recurring `restore-drill` path instead allocates a distinct test-only database
and applies the embedded migrations plus pinned scratch-only legacy archive DDL
on each invocation. Its protected absolute evidence root must already exist.
It preserves previous targets, quarantines imported expiries and retains private
no-overwrite receipts and an archive copy. Inspect receipt `dropped_columns`:
matching counts do not prove every source column survived. Failures retain their
target/evidence too. Existing S6 grants can apply to an already-present runtime
group in the new test DB; no new roles/credentials or special drill grants are
added. Neither path starts a gateway or enables moderation. Unknown restore
options refuse (exit 2).

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

The mock-only staging-guild E2E prep skeleton
(`scripts/staging_guild_e2e_prep.py`, stdlib only, no network, no credentials)
drives the guild interactions-endpoint shape against local fixtures: identity,
the guild command list, then one per-command resource read per surface. A live
guild id aborts before anything is built or sent; without `--mock` there is no
transport at all. The offline regressions (`scripts/test_staging_guild_e2e_prep.py`)
prove the live-guild refusal and run in CI on standard runners. The live
Discord run against the staging guild arrives with the full staging suite;
this skeleton never touches staging. Run the mock-only prep with
`scripts/staging_guild_e2e_prep.py --mock` (local fixtures, no network); a
live guild id exits 2 with nothing sent.

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
| DB unreachable / missing schema or grants | `DATABASE_URL` is required. Gateway connect/hydration/checkpoint failure can exit the process; its SQL error is withheld. Gateway and lazy jobs are DML-only; runtime will not provision schema. Job failures may coexist with ready gateway state, while the live `database` ping can independently make `/readyz` 503. Follow the [Neon/Hyperdrive playbook](#neon-or-hyperdrive-outage); repair the named dependency through its owner, not another credential, SQL probe or widened grant. |
| HTTP 200 health but persistent 503 ready | Listener works; inspect `gateway`, `database` and `token_invalid` state and sanitized logs. A database failure or rejected-token latch need not be a gateway transport outage. Never soften readiness or count the scaffold-era deploy gate as recovery. |
| Reconnect / RESUME refused | Follow [restart semantics](#restart-semantics-durable-resume-not-full-state-recovery); 4007/4009 force fresh IDENTIFY. Preserve the durable checkpoint, don't hand-edit sequence or start another shard. |
| Discord REST 429 / suspected breaker | Separate token-wide durable admission, executor-local pacing, process-wide global pause/invalid-request breaker, and the private announcement governor. Refusal can precede HTTP; retry bounds vary by action. There is no manual reset endpoint. Do not hammer Discord, replay uncertain moderation writes or restart/delete state to clear a hold. Identify the actual writer and use verified containment; see the [Discord playbook](#discord-gateway-or-api-outage). |
| Channel moderation lane stuck `in_progress` after an ambiguous write | No automatic retry/expiry. Quiesce original workers, establish old REST settlement, and read actual Discord overwrites/slowmode before using the inspection-first, explicitly confirmed operator CLI. It preserves recovery and audits the prior claim. See [channel lane reconciliation](channel-lane-reconciliation.md); never release a lane while a delayed unlock can still write. |
| Website event action stuck `needs_reconciliation` after an ambiguous create | No automatic retry: re-submitting under a new key can make a second event. Read the actual guild scheduled events in Discord, then resolve the exact intent with the inspection-first, explicitly confirmed `two-bot reconcile-event` CLI (`--list`, then `--created`/`--updated`/`--cancelled`/`--no-effect` with `--execute`). See [the receiver contract](internal-actions-receiver.md); never re-submit the operation under a new key before reconciling. |
| `POST /internal/actions` 404 on staging | The route is dark unless the Worker var `INTERNAL_ACTIONS_INGRESS` (staging env) **and** the secret `TWO_INTERNAL_ACTIONS` are both exactly `1`. Wrong method, a trailing slash or any query string is also 404 by design. Production is always 404. |
| `POST /internal/actions` 503 `unavailable` | The container is not running (public ingress never starts it; wait for the probe or keepalive), the ownership fence refused this deployment, or the receiver answered something other than its JSON envelope. Check `/readyz` and ownership status; do not retry-loop a signed request with a new nonce. |
| Ready but feature inactive | Gateway readiness says nothing about library-only commands/jobs/kill switches. Check [runtime boundaries](#containment-kill-switches-and-feature-flags), not extra environment guesses. |
| Worker restored but Rust regression remains | Worker-version rollback did not prove image rollback. Inspect the active image and use a schema-compatible full redeploy of the known-good pair. |
| Backup/drill red | Preserve valid archives; inspect exit status, verifier line, table counts and off-box receipt. Rehearse only on a prepared test database. No automatic promotion to a production restore. |

## Incident playbooks

Current source contracts checked at `4091837cb4dd8dea9a6ebe4e2b1dfcf21e9b2a45`;
re-check the affected deployment if its revision differs. The October-1 tabletop
below is historical evidence at its own older baseline, not a current wiring
inventory. Neither source verification nor these documentation corrections prove
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
The [October-6 staging record](incident-tabletop-2026-10-06.md) is the Discord and
Neon dry run against the live staging baseline, with its open evidence gaps.
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
REST symptoms). Use an existing authorized internal scrape or the approved
Worker operations proxy described in [health/metrics exposure](#is-it-alive).
Unauthenticated `/metrics` is not proxied; the authenticated operations path is
not a no-start observation and must not be used to test containment.
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
4. Determine which actual runtime is sending REST requests and whether a
   refusal happened before HTTP. The shipped command/job constructors use
   durable admission, and transport attempts share a process guard. Stop
   operator-initiated send/replay work; preserve ambiguous outcomes rather than
   repeating moderation, announcement or role writes.

Command and job executors use token-wide durable `PgSendAdmission`. A shared
held/cooldown lane or unavailable admission database can refuse a send **before
HTTP**; inspect [send-admission alerts](#alert-send-admission-blocked) and the
safe refusal class before calling that a Discord outage. This durable lane is
separate from executor-local pacing: clones share their executor's pacing state,
while separately constructed executors share a credential-derived token lane
only when they use the same authoritative database. A held durable lane does
not expire or clear on process restart.
The paced floor is 110 ms (kick floor 350 ms). Sticky message POST and interaction
callback/edit use single five-second attempts without that pacing path; a sticky
POST failure releases its claim so later message activity can try again.
Registry publication has at most five sends for repeated 429/5xx. Paced job GET
429 retries do not consume the attempt count and rely on the outer job deadline.

Ordinary production transports also share the process-wide `process_guard` for
global pauses and the invalid-request circuit breaker. Attempts check that guard,
so intentional delay/refusal may precede HTTP even when local pacing is free.
The guard is neither token-keyed nor cross-process/durable. Essential interaction
acknowledgements bypass only the invalid-request budget, not a global pause or
`token_invalid` latch. A bot-authenticated 401 latches token refusal; that is a
credential incident, not a reason to restart merely to regain REST admission.

The private announcement executor instead uses durable admission plus its own
clone-shared cooldown governor, **not** `process_guard`. Its typed reconciliation
method is not an operator reset endpoint or a durable-lane reset. There is **no
manual reset endpoint** for these runtime controls. Do not clear a hold by
restart, deletion, another token or a burst of probes. These controls limit
sends; none is a bot-wide persistent incident fence. Preserve uncertainty rather
than promising no later retry.

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
[`gateway.rs`](../crates/bot/src/gateway.rs),
[`gateway_metrics.rs`](../crates/bot/src/gateway_metrics.rs),
[`metrics.rs`](../crates/core/src/metrics.rs),
[`command_runtime.rs` admission wiring](../crates/bot/src/command_runtime.rs),
[`website_jobs.rs` job executors](../crates/bot/src/website_jobs.rs),
[`PgSendAdmission`](../crates/core/src/send_admission/postgres.rs),
[`ActionExecutor / HyperTransport`](../crates/discord/src/executor.rs),
[`process_guard`](../crates/discord/src/ratelimit_guard.rs),
[`AnnouncementExecutor`](../crates/discord/src/internal_actions.rs),
[`announcement cooldown governor`](../crates/discord/src/internal_actions/governor.rs),
[Discord connection lifecycle](https://docs.discord.com/developers/events/gateway#connections).

### Neon or Hyperdrive outage

**Detection.** Gateway Postgres uses the forwarded `DATABASE_URL` directly.
Hyperdrive `REDIRECT_DB` is a separate redirect binding, **not** the Rust
connection path. With `REDIRECT_DB`, the Worker supplies `connectPostgres` to
`RedirectStore`: live lookup and click insertion are implemented. Without that
binding, the store serves the configured snapshot and drops non-live click
records. A failed live lookup uses the configured fallback invite, or 503 when
none is valid; it does **not** switch to the mappings snapshot. Click insertion
is asynchronous and can fail after a redirect response. Thus a working redirect
does not prove attribution or either DB is healthy. Identify which branch the
**deployed** environment uses before diagnosing an outage; source support alone
does not prove a live binding.

Use literal `durable gateway failed; checkpoint unchanged, readiness unavailable`
(no underlying SQL error), `periodic job failed` (`job`, `error_class=database`),
and `sticky lookup failed; skipping activity` /
`sticky claim failed; skipping activity` (`error`, redact before sharing).
The informational
`jobs` readiness object records `last_error_class`, `consecutive_failures` and
`last_success` (epoch **milliseconds**); job failures do not change the HTTP
ready status. Every supervised job success updates
`two_bot_job_last_success_timestamp_seconds{job}` (epoch **seconds**) and clears
that job's consecutive-failure count. This includes successful no-op/skipped
ticks, so it is not proof of fresh scorecard publication or other external data.
Metric job labels are bounded; unlisted jobs collapse to `other`.
`two_bot_db_errors_total{op="admission"}` records send-admission storage failures,
not all SQL failures. `two_bot_db_pool_configured`, `two_bot_db_pool_connections`,
`two_bot_db_pool_idle_connections` and `two_bot_db_pool_max_connections` sample
the shared feature **Store pool**, not the separate gateway checkpoint pool or
all lazy job pools. Pool availability gauges are not DB connectivity probes.

The `/readyz` `database` component performs a bounded live ping (two-second
probe timeout) against the registered Store pool. Missing pool, failed ping or
timeout produces `database=down` and HTTP 503 even when `gateway=ready`.
`token_invalid` is also a readiness component; `down` means a bot-authenticated
401 has latched refusal, not a Neon outage. The ping does not validate every
feature's schema/grants or another pool. Use the actual component breakdown and
existing authorized [metrics exposure](#is-it-alive); never expose metrics
publicly. Preserve sanitized evidence, not URLs or unredacted SQL errors.

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
   active dependency. If `REDIRECT_DB` is present, separate a failed live lookup
   (which may serve snapshot fallback) from a failed click insert. If absent,
   record snapshot-only/no-live-attribution as the configured boundary rather
   than an outage. In neither branch is redirect `/healthz` gateway-DB proof.
   Do not manufacture a campaign click to exercise a production DB.

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
`presence_probe`, `community_scorecard`, `inactivity` jobs can fail `database`
while the gateway component remains ready. Ordinary job failure is recorded and
cadence continues. The scorecard has at most three durable attempt slots, five
minutes apart from each reservation, **not three retries**. Eligibility remains
Monday 06:15–before 07:00 UTC. Lazy pool acquisition occurs **before** reservation:
a connection failure there consumes no slot; after a committed reservation,
failure/cancellation/crash retains the consumed slot. Restart does not reset the
budget, and an already persisted run (even ingestion-incomplete) is reconciled
as completed. Same-window recovery is possible only if time, budget and
completion state still allow it; do not promise an immediate retry.

Gateway connect is DML-only; lazy jobs also use `skip_migrations=true` and perform
no web-contract DDL. The operator provisioning path must install migrations,
contract tables/views and approved grants before startup. Schema/grant failures
are not evidence that runtime will repair them; route the discrepancy to the
named dependency/runtime owner. No widening grants or executing migrations is
authorized by this runbook.

Retain the last verified off-box archive and its receipt; an outage is not proof
of corruption. The current writer is v4: [`DUMP_TABLES`](../crates/core/src/backup/dump_file.rs)
includes `gateway_sessions` and bot-owned website backing tables as well as
funnel/projections. The first 22 tables are the frozen **v3 prefix**, not the
current coverage limit. Inspect the actual archive version, complete manifest,
column/type metadata and counts; do not infer coverage from its filename.
Derived views and website-owned data are not included, and this is not a full
Neon, Discord or Hyperdrive configuration restore. See [backup formats](backup.md).
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
outcome; readiness alone is insufficient. For a live `REDIRECT_DB` deployment,
require the authorized journey's lookup **and** persisted attribution evidence;
a fallback redirect is not proof of either. For a deployment without that
binding, record the snapshot-only/no-live-attribution boundary and do not claim
Hyperdrive recovery. Gateway persistence requires its own evidence in both cases.
Do not use a SQL test, migration or destructive restore as the verification step.

**Post-incident record.** Save the affected direct-DB/Hyperdrive surface,
non-secret environment/branch/binding identity, safe log fields, last committed/
first recovered evidence when available, deployment provenance, provider link,
actual degraded/refused behavior, uncertain external effects and attribution
loss. Record whether backups were only preserved, file-verified, or restored
under a separate approval; include data gap/RPO/RTO only when evidenced. Name
repair and follow-up owners. Do not declare full recovery from `/health` alone.

Source: [`gateway.rs`](../crates/bot/src/gateway.rs),
[`gateway_session.rs`](../crates/cutover/src/gateway_session.rs),
[`website_jobs.rs` lazy connection](../crates/bot/src/website_jobs.rs),
[`community_jobs.rs` reservation ordering](../crates/bot/src/community_jobs.rs),
[`community_scorecard_retry.rs`](../crates/bot/src/community_scorecard_retry.rs),
[`jobs.rs` supervised outcomes](../crates/bot/src/jobs.rs),
[`server.rs` readiness ping](../crates/bot/src/server.rs),
[`main.rs` Store-pool registration](../crates/bot/src/main.rs),
[`metrics.rs`](../crates/core/src/metrics.rs),
[`Worker redirect wiring`](../wrangler/src/index.ts),
[`RedirectStore`](../wrangler/src/redirect-store.ts),
[`redirect.ts` lookup-failure fallback](../wrangler/src/redirect.ts), [backup contract](backup.md).

### Suspected bot-token compromise: containment only

**Detection.** A report of token exposure or unexpected actions by the bot
identity is sufficient to start triage; do not paste the leaked value or test it.
Credential rejection may cause generic
`durable gateway failed; checkpoint unchanged, readiness unavailable`, not a
dedicated compromise signal. `gateway prerequisites missing; gateway parked,
/readyz reports down` (`missing`, `status`) describes a newly parked runtime,
not revocation or stopping an older process. Existing private
`two_bot_rest_requests_total{route,result="4xx"}` can corroborate rejections but
does not distinguish 401/403 or prove compromise. A bot-authenticated REST 401
emits `Discord refused the bot token; REST disabled until restart` and latches
`token_invalid=down` in readiness. That is rejection evidence, not proof of
compromise or permission to restart/replace a credential. Health/readiness,
metrics and reconnect counts cannot prove a token has not been copied. Preserve
sanitized deployment/guild audit references and available lifecycle evidence,
not raw credential-containing logs.
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
3. Request authorized **Worker-side containment** of the affected singleton
   `two-bot` using the implemented [persisted ownership control](#persisted-ownership-control).
   For an authorized staging operation, use that covered client's no-start
   `status`, then `fence` with the exact readback epoch and audit actor, then
   `status` again. This is a gated operational action, **not a tabletop step**.
   Confirm the deployed version, DO namespace and existing dedicated control
   binding before acting; a missing/denied binding stops this path. Do not
   substitute credentials, expose a public stop route, delete DO storage or
   call native lifecycle methods outside the covered control procedure.
4. Have the **owner/authorized credential custodian**, not an agent, revoke the
   compromised bot token in the [Discord Developer Portal](https://discord.com/developers/applications).
   Discord's Bot-page **Reset Token** invalidates the old token and issues a
   replacement: the entire action is owner-only credential rotation, not an
   agent-executed revocation workaround. Do not copy the old or replacement
   token into a ticket, argument, log, PR, chat or archive.

**Containment boundary / shipped control.** The persistent ownership fence is
implemented. Authenticated control durably parks ownership (`deploymentId=null`,
`phase=fenced`) before awaited native container teardown and removes keepalive
schedules. Ownership admission covers health/readiness probes, authenticated
`/ops/metrics` proxy auto-start, keepalive and direct SDK start entries; disabling
just two observed routes is not equivalent. After a successful revocation write,
teardown failures leave the owner fenced; a failed initial storage write is not
proof that persisted ownership changed. Denial alone is **not evidence that the
old process stopped**.
Require authenticated control readback **and** confirmed `running=false` with
no-revival evidence. The staging client rejects production origins: production
containment requires its separately reviewed execution sheet/API contract, not
an adapted staging command or inferred approval.

A single process kill or SDK `stop()`, an audit-library halt, disabling a health
monitor, or `KEEPALIVE_SECONDS=0` is not persistent containment (zero falls back
to 60 seconds). The fence coordinates only the verified DO namespace/singleton;
it does not stop a legacy process, another namespace or an attacker using a
copied token. Custodian revocation remains necessary. If deployed control,
readback or teardown cannot be verified, report **incomplete containment** and
the exact evidence/access gap to the engineering manager; do not implement an
unreviewed emergency control or treat a 503 as success.

**Containment verification, not restart.** Use the authorized no-start ownership
status and existing platform evidence to confirm `owner.phase=fenced`, new
`owner.epoch`, `owner.deploymentId=null`, native `running=false`, removed keepalive schedules and no
old-identity process in the affected scope. Do not use `/health`, `/readyz` or
`/ops/metrics` as no-start readback: they can start an eligible unfenced runtime.
Observe logs/platform state over at least two previous keepalive intervals and
record stop time, scope and no-revival evidence. Obtain the custodian's names-only
revocation receipt; do **not** authenticate with the old token as a test. If
shutdown/no-revival or revocation is unverified, containment remains incomplete.

**Recovery handoff / post-incident record.** This playbook ends with verified
containment and custodian handoff, **not automatic restart**. The owner performs
rotation and approved secret provisioning; agents must not create, delete,
rotate, export or install a replacement credential. Restart needs its separate
reviewed deployment, security/access gate, known-good version/image pair and
staging verification. Record exposure/containment/revocation times, non-secret
identity/surfaces, affected actions and uncertain damage, preserved redacted
evidence, actual Worker-stop/no-revival receipt, any missing controls, and the
security/owner recovery handoff. Never interpret silence as rotation approval.

Source: [`ownership.ts` durable fence transition](../wrangler/src/ownership.ts),
[`Worker lifecycle, control and SDK entry guards`](../wrangler/src/index.ts),
[`staging ownership client`](../wrangler/scripts/ownership-control.mjs),
[`main.rs` gateway supervision](../crates/bot/src/main.rs),
[`metrics.rs`](../crates/core/src/metrics.rs),
[`ratelimit_guard.rs` rejected-token latch](../crates/discord/src/ratelimit_guard.rs),
[`server.rs` shutdown and readiness](../crates/bot/src/server.rs),
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
| Staging private receiver (default dark) | `TWO_INTERNAL_KEYS` (signing key), `TWO_INTERNAL_CALLERS`, `TWO_INTERNAL_CHANNEL_KEYS`, `TWO_INTERNAL_ACTIONS` | Staging Worker secrets set by the Operator only, never `wrangler.toml` vars and never in production. `TWO_INTERNAL_ACTIONS=1` is set last. The key is generated on the Operator host and never printed; see the [enable order](internal-actions-receiver.md#enable-order-and-rollback). |
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
| Alive/logs/restart, DB health, SIGTERM | Replaced by health/readiness, Worker/container observations and durable restart above. Readiness includes a bounded Store-pool database ping and token-refusal state; legacy ports and npm start do not apply. |
| Redeploy/rollback/Coolify deployment | Replaced by explicit Wrangler environment/version/image procedure. Current production cutover remains separate; no Coolify action is prescribed here. |
| Backup/restore, off-box storage, timers | Ported CLI/unit templates; [backup.md](backup.md). Only test-container rehearsals authorized here; no assertion timers are installed. |
| Guild-config snapshot/restore/seal/drift | Ported staging-guarded CLI; not permission to run a real restore or use a live token. |
| Operational audit kill switch/MAC reasons | Core/store port exists; delivery/operator control not wired. No fake SQL/CLI replacement. |
| Automod, moderation, feature toggles | Forwarded gates, channel-command handlers and gateway automod are implemented and identity/permission gated. This is not blanket builtin coverage or cutover approval; use the current containment table and verify the deployed writer. |
| Health/REST rate limits/429 breaker | Liveness/readiness, durable token admission, executor-local pacing and process-wide guard are implemented. They differ from the legacy control topology; no manual reset endpoint. The private announcement receiver has a separate governor; a legacy writer uses its own runbook. |
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
`ci-ok` (the full verdict over lint, worker checks and all selected Rust/DB test
lanes), `worker check`, `pr-lint`, and `gitleaks`; a docs test is not a waiver.
