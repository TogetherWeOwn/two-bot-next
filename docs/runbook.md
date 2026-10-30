# two-bot-next operations runbook

<!-- start-here -->
**Start here — section map**

- [Start here](#start-here)
- [Is it alive?](#is-it-alive)
- [Persisted ownership control](#persisted-ownership-control)
- [Redeploy and rollback](#redeploy-and-rollback)
- [Restart semantics: durable RESUME, not full state recovery](#restart-semantics-durable-resume-not-full-state-recovery)
- [Containment, kill switches and feature flags](#containment-kill-switches-and-feature-flags)
- [Backup, restore and drill commands](#backup-restore-and-drill-commands)
- [Common failures](#common-failures)
- [Incident playbooks](#incident-playbooks)
- [Secret inventory: names only](#secret-inventory-names-only)
- [Legacy review and deliberately unported operations](#legacy-review-and-deliberately-unported-operations)
- [Keep the runbook honest](#keep-the-runbook-honest)
- [Giveaways](#giveaways)

**Alert anchors**

- [job stale](#alert-job-stale)
- [job unknown stale](#alert-job-unknown-stale)
- [job failures](#alert-job-failures)
- [job failure share](#alert-job-failure-share)
- [REST 429](#alert-rest-429)
- [REST 5xx](#alert-rest-5xx)
- [REST admission breaker open](#alert-rest-admission-breaker-open)
- [REST token invalid](#alert-rest-token-invalid)
- [REST refusal surge](#alert-rest-refusal-surge)
- [REST global responses stuck](#alert-rest-global-responses-stuck)
- [DB pool](#alert-db-pool)
- [DB errors](#alert-db-errors)
- [send admission blocked](#alert-send-admission-blocked)
- [voice failures](#alert-voice-failures)
- [voice join fast burn](#alert-voice-join-fast-burn)
- [voice join slow burn](#alert-voice-join-slow-burn)
- [voice creator-map stale](#alert-voice-creator-map-stale)
- [voice worker halted](#alert-voice-worker-halted)
- [voice live self-heal](#alert-voice-live-self-heal)
- [voice live not ready](#alert-voice-live-not-ready)
- [voice queue stall](#alert-voice-queue-stall)
- [error budget fast burn](#alert-error-budget-fast-burn)
- [error budget slow burn](#alert-error-budget-slow-burn)
- [gateway missed events](#alert-gateway-missed-events)
- [gateway unpaired disconnect](#alert-gateway-unpaired-disconnect)
- [gateway not ready](#alert-gateway-not-ready)
- [gateway latency dead](#alert-gateway-latency-dead)
- [gateway reconnect flapping](#alert-gateway-reconnect-flapping)
- [ticker stale](#alert-ticker-stale)
- [receiver refusals](#alert-receiver-refusals)
- [checkpoint failures](#alert-checkpoint-failures)
- [dispatch drops](#alert-dispatch-drops)
- [handler duration saturated](#alert-handler-duration-saturated)
- [prefix trigger verdict refusals](#alert-prefix-trigger-verdict-refusals)
- [community facts drain failures](#alert-community-facts-drain-failures)
- [EventSub post failed share](#alert-eventsub-post-failed-share)
- [event-store consumer lag](#alert-event-store-consumer-lag)
- [metrics scrape lost](#alert-metrics-scrape-lost)
- [audit delivery halt](#alert-audit-delivery-halt)
- [eventsub reconcile failed](#alert-eventsub-reconcile-failed)
- [EventSub tampered](#alert-eventsub-tampered)
- [gateway drain skips](#alert-gateway-drain-skips)
- [retention replay needed](#alert-retention-replay-needed)
- [voice dispatch timeouts](#alert-voice-dispatch-timeouts)
- [voice watchdog stucks](#alert-voice-watchdog-stucks)
- [voice halt recoveries](#alert-voice-halt-recoveries)
- [REST breaker opens](#alert-rest-breaker-opens)
- [REST global pauses](#alert-rest-global-pauses)
- [feed fetch failures](#alert-feed-fetch-failures)
- [EventSub receive gap](#alert-eventsub-receive-gap)
- [EventSub reconcile prior failed](#alert-eventsub-reconcile-prior-failed)
- [automod sanction refused](#alert-automod-sanction-refused)
- [moderation enforcement failures](#alert-moderation-enforcement-failures)
- [voice REST stuck](#alert-voice-rest-stuck)
- [voice global backoff capped](#alert-voice-global-backoff-capped)
<!-- /start-here -->

For the on-call operator of the Rust bot and its Cloudflare Worker/Container.
The bot is live in production on one Cloudflare Container; this describes the
shipped source, not proof that a soak passed or that any specific recovery is
complete. Production restores, token rotation, and live-guild changes need their
separate authorization.

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
Wrangler 4.152.0 through `wrangler/package.json`. Use only the already-authorized
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

For read-only admin change history, see the compact [ops audit contract](ops-control.md)
(response allowlist, byte/row bounds, refusal actions and offline checks).

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
jobs map (freshness-graded when ready or with --expect-ready, otherwise
informational), and that the body is the container's breakdown rather
than an ownership-fence refusal. Pass `--json` for one machine-readable
verdict object with stable per-check ids, codes and next actions.
Malformed origins and remote fields fail as bounded fixed-vocabulary
verdicts that echo nothing; failures never write `--evidence`, which stays
a success-only attestation. If the checks pass but the receipt write itself
fails, `--json` reports FAIL with an `evidence` row instead of PASS.

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
logs in the Cloudflare dashboard. Wrangler 4.152.0 has no `containers logs`
subcommand; do not invent one. Container inspection may list account-wide
resources: match the affected environment/application before taking any action.
Container stdout is also in the Workers Observability `containers` dataset, which a
read-only `CLOUDFLARE_API_TOKEN` can query without starting the container:
`python3 scripts/container_stdout_read.py --service <container-application-id>`
(see [Reading container stdout](startup-diagnostics.md#reading-container-stdout)).
That script keeps each line's message and a fixed allowlist of fields that excludes
`voice_event`; it can find a voice line by message, and the
[staging voice-event verification](staging-voice-event-verification.md) dashboard procedure
reads the `voice_event` field.

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
  `error_class` plus the failing step and SQLSTATE code are also on `/readyz`
  as `gateway_failure` for 15 s before exit (startup failures linger 15 s;
  only a running-gateway failure lingers 75 s) and in Workers Logs as
  `container_gateway_failure` (see [startup-diagnostics.md](startup-diagnostics.md)).
- `container service failed` / `SIGTERM received; draining`.
- Worker: `two-bot container started|stopped`, `container_readyz_unhealthy`,
  `container_keepalive_probe_failed`, `container_error`.

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

#### Alert: job unknown stale

A job without a staleness cadence entry recorded no successful completion for
more than 2 hours (`two_bot_job_last_success_timestamp_seconds{job}`). This is
a **ticket, not a page**: without a mapped cadence the monitor cannot tell a
slow-but-healthy schedule from a wedge, so it fails closed and nags for an
explicit cadence, ticker coverage, or event-driven exclusion instead of
staying silent. `session_checkpoint` is event-driven (one success per durable
gateway commit) and is excluded from this rule in
`UNKNOWN_STALE_EXCLUDED` (`wrangler/src/alert-rules.ts`): it never tickets
however stale. A job that never succeeded since start (timestamp zero) is not
reported here: that covers both boot and parked jobs.

First response: check the `jobs` map on `/readyz` for the job's `parked`,
`last_success` and `consecutive_failures` fields, then read the
Worker/container logs for `periodic job failed`. A parked job with a zero
timestamp is configuration, not a wedge. The standing fix is an explicit
cadence entry, ticker coverage, or event-driven exclusion for the job — a
reasoned catalog note alone does not stop the ticket — until then this ticket
re-fires while the timestamp stays stale. Restart only after the
logs show the job loop is wedged, per the [restart semantics](#restart-semantics-durable-resume-not-full-state-recovery).

Escalate when staleness persists after the suspect deploy or dependency is
identified, or when it coincides with pool-saturation or DB-error alerts —
the fix then belongs to the on-call engineer, not another redeploy.

#### Alert: job failures

A job failed three completions in a row. The page includes the latest failure
class when the `two_bot_job_last_error_class` series is available. If it is
missing (for example, on an older build), the page still fires without a class;
use sanitized `periodic job failed` logs instead. Error classes are fixed enum
names, never error text or payloads. Take the first action for the reported class:

| Class | First action |
|---|---|
| `database` | Check the database dependency and expected schema/grant state through its authorized owner; do not run SQL probes or widen grants. |
| `rest` | Check Discord status and REST metrics for 429, 5xx or transport failures; honor `retry-after` and do not replay uncertain writes. |
| `configuration` | Compare non-secret binding names and feature gates with the reviewed deployment; do not dump values or substitute credentials. |
| `timeout` | Check the job status and pool-pressure metrics to locate the bounded operation before considering a restart. |
| `panic` | Correlate the sanitized failure log with the deployed revision; the panic payload is intentionally unavailable. |
| `feed` | Check the configured feed source's availability and recent fetch status through authorized diagnostics; do not copy response bodies. |
| `recovery_required` | Preserve feed cursor and pending recovery state; follow the established recovery path before any manual replay or cursor change. `/feed-remove` refuses while pending delivery evidence is unresolved, so a parked poller or deleted channel blocks removal until recovery can run; no force path exists. |

The streak clears on the next successful completion. For feed delivery
specifically, follow the [feed supervisor operator guide](feed-supervisor.md).

#### Alert: job failure share

One job's `two_bot_job_runs_total` failures exceeded 10% of its recent runs:
at least 10 completed runs with at least 2 failures in the judged window.
This is the intermittent-sickness ticket. [`job_consecutive_failures`](#alert-job-failures)
pages when a job fails three times in a row, but a job failing every other
run never trips it; this rule accumulates runs across consecutive valid
samples until the 10-run floor is met and then judges the share, so the
flapping poller, sweeper or snapshotter tickets here. Sub-floor windows hold
the baseline and accumulate instead of judging, so slow-cadence jobs (an
hourly probe needs ~10 hours to judge) ticket late but never on thin
evidence. Only a judged window at or below the line resolves; flat and
sub-floor windows hold an active ticket, and a counter reset (process
restart) rebaselines and resolves. The first valid sample only stores the
baseline and never fires; missing or malformed samples preserve the firing
state and the baseline. Unknown job labels are ignored, never subjects. This
is a **ticket**, not a page: the consecutive-failures page owns the
continuous outage. The firing key carries the job subject
(`job_failure_share:<job>`).

A job dead enough to page also tickets here: co-firing with the
consecutive-failures page for the same job means continuous outage — follow
the page first. This ticket alone means intermittent failure: successes land
between the failures, so the cause is flakiness (a forking dependency, a
racy tick, a poisoned-then-recovered lock) rather than a hard wedge. A job
that stops completing anything holds this ticket while the staleness rules
(`job_stale`, `ticker_stale`, `job_unknown_stale`) fire on its aging success
timestamp; treat that pair as a wedge, not flakiness.

First response: scrape `/ops/metrics` as described above and read the job's
`success`/`failure` run counts, its `consecutive_failures` streak and its
one-hot `last_error_class` row — the class names the failure family the
share alone cannot. Then read the Worker/container logs for `periodic job
failed` lines for that job over the judged window and correlate with deploy
and dependency health. Do not restart the container to "clear" the share; a
replacement resets the baseline without healing the flake, and the next
judged window re-tickets.

Escalate when the ticket co-fires with the consecutive-failures page (the
outage turned continuous), when several jobs ticket at once (the cause is a
shared dependency, usually the database or the Discord REST path), or when
the ticket survives the suspect deploy's rollback — the fix then belongs to
the on-call engineer, not another redeploy.

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

Executor route templates (fixed vocabulary): `GET /guilds/:guild/members/:member`
counts member fetches, `GET /users/@me` counts current-user reads (literal
`@me`, never a user id), `PATCH /webhooks/:application/:token/messages/@original`
counts interaction-response edits (literal `@original`). Snowflakes, interaction
tokens and query strings never appear in labels: `guilds/100/members/300?limit=1`
counts as the member template, `users/@me?limit=100` as `GET /users/@me`,
`webhooks/7/tok-abc/messages/@original?wait=true` as the webhook template.
Unknown, wrong-method and overlong shapes collapse to `other` with no new
series. Source: [`metrics.rs`](../crates/core/src/metrics.rs),
[`executor_metrics.rs`](../crates/discord/src/executor_metrics.rs).

#### Alert: REST 5xx

More than 10% of Discord REST requests between two keepalive samples (minimum
10 requests in the window) returned 5xx. Unlike 429s, a 5xx burst is never
healthy backpressure: it is Discord-side failure or a broken request shape
from a fresh deploy. A counter reset (process restart) skips the window rather
than firing. `transport` failures (timeouts and cancellations before response
headers) are not counted here; they surface through job timeout classes.

First response: read the hot route from the `route` label on
`two_bot_rest_requests_total{route,result="5xx"}` via the authorized
`/ops/metrics` scrape; confirm whether a deploy or promotion is in progress
(check the staging workflow result and recent merges — a fresh deploy can
explain a new hot route). Do not replay uncertain writes: a 5xx leaves the
effect unknown, so reconcile through the recorded disposition before retrying
anything the burst may have executed twice.

Evaluate rollback when the burst starts with the deployed revision: compare the
alert's first-firing window with the staging-rollout evidence (or the gated
production run) for that SHA. A 5xx share introduced by a promotion stage
requires a manual rollback decision unless the staging `error_rate_gate`
step invokes the `evaluatePromotionGate` helper
(`wrangler/src/alert-rules.ts`, 5xx-only, via
`wrangler/scripts/promotion-error-gate.mjs`) and fails on its verdict. When
that gate fails, the existing staging auto-rollback restores the recorded
pre-promote Worker version and container image; a manual rollback names the
recorded Worker version and container image from the failed run's snapshot.
The gate enforces live traffic only once the `STAGING_METRICS_SCRAPE_TOKEN`
secret is provisioned — without it the step records a skip, and the
dispatch-only `inject_error_rate_spike` input judges a rehearsal firing to
prove the rollback chain (see `docs/runbook-observability.md` for the exact
coverage limits). A live window with fewer than 10 REST requests records
`insufficient_data`, never a pass, so only a `pass` with `rest_requests >= 10`
counts as enforcement proof. Production has no error-rate gate: a burst on the
production revision still needs a manual rollback decision. A burst on an
unchanged revision is a Discord incident or a dependency regression, not a
rollback.

Escalate when the 5xx share stays above threshold across several windows after
containment, or when 5xx coincide with 429/transport failures suggesting a
wider Discord or network incident.

#### Alert: REST admission breaker open

The invalid-request breaker stayed open (`two_bot_rest_guard_breaker_open` 1)
for three consecutive valid keepalive samples. This is a **ticket**, not a page:
the guard refuses non-essential REST sends locally. Essential interaction
acknowledgements may bypass this breaker, but never a global pause or fatal
token state. Local refusals happen before any REST wire attempt, so the
request counter (`two_bot_rest_requests_total`) and its 429/5xx alerts cannot
see this condition — a quiet 429/5xx board alongside this ticket is the
expected shape of a local-refusal episode, not proof the REST path is healthy.
Global pauses (`two_bot_rest_guard_global_paused`) report separate backpressure
and are not this rule's input. However, every 429 response, including global
429s, counts toward the same 600 s invalid-request window and can contribute
to opening the breaker.

First response: read the guard's `discord_breaker_open` log line for the
invalid-request count and threshold, plus `two_bot_rest_guard_invalid_requests_in_window`
and `two_bot_rest_guard_rejected_requests_total` from the authorized
`/ops/metrics` scrape; confirm whether a deploy is in progress (a fresh
deploy shipping a bad token or a hot invalid route fills the window fast).
There is no manual breaker reset: the breaker closes on its own when the
10-minute rolling invalid-request window clears below threshold, so do not
restart the container to "clear" it — a restart only resets the counters and
destroys the evidence. Missing, malformed or failed scrapes break the sampling
streak but hold any firing ticket; only a valid closed sample resolves it.

Check `two_bot_rest_guard_token_invalid` and the `/readyz` `token_invalid`
component separately. That fatal latch refuses all new REST attempts, including
essential acknowledgements. Once outstanding responses finish and the window
drains, the breaker closes and this ticket resolves even while the token latch
stays set and readiness stays **503**. **Ticket resolution is not REST recovery.**
Follow the [fatal-token procedure](rest-guard.md#fatal-bot-token-and-readiness):
the authorized credential operator verifies provisioning/token validity, then
restarts with corrected configuration. Waiting for the window cannot clear
the token latch; never automatically rotate a token or bypass the guard.

Escalate when the breaker stays open across windows after the rolling window
should have cleared, when invalid requests keep arriving with no suspect
deploy (a caller may be minting bad requests in a loop), or when the ticket
coincides with missed gateway events or 5xx alerts suggesting a wider Discord
or network incident.

#### Alert: REST token invalid

The bot-token latch read `1` (`two_bot_rest_guard_token_invalid`). A
bot-authenticated 401 permanently latched `token_invalid` for this process:
every new REST attempt, including essential interaction acknowledgements,
refuses locally, and `/readyz` reports 503 until a restart with a corrected
token. This pages on the first valid latched sample with no streak bound: the
latch defaults to `0` at boot and is set only by a bot 401, so a `1` is never
a boot artifact. A 401 on an interaction callback or original-response webhook
edit counts as invalid but does not condemn the bot token (short-lived
interaction token), so this page always means the bot credential itself. A
valid `0` resolves (only a new process without the latch reads `0`); missing,
malformed or failed scrapes hold the page without manufacturing recovery.

First response: stop retries at once — every retry refuses and burns no
budget, but it pages the same incident. Read the `discord_token_invalid` log
line and confirm `/readyz` shows `token_invalid` down via the keepalive
breakdown. Then follow the [fatal-token
procedure](rest-guard.md#fatal-bot-token-and-readiness): the authorized
credential operator verifies provisioning/token validity under the normal
credential approval procedure, then restarts with the corrected configuration.
Never log, swap in an unrelated credential, or automatically rotate a token.
Expect [REST refusal surge](#alert-rest-refusal-surge) alongside: token-driven
refusals grow that counter too, so treat the token page as the cause and the
surge as its echo.

Escalate when the latch re-arms right after a corrected restart (provisioning
disagrees with the running configuration), or when the page coincides with a
wider identity incident — the fix belongs to the credential owner, not another
redeploy.

#### Alert: REST refusal surge

Combined guard invalid observations (401, 403 and 429 at response headers)
plus local refusals (breaker, global-pause and token refusals, pre-wire)
grew by 10 or more between two keepalive samples
(`two_bot_rest_guard_invalid_requests_total` plus
`two_bot_rest_guard_rejected_requests_total`). Healthy windows sit at zero,
so 10 in one window is a loop, a bad deploy, or a Discord-side storm — never
one operator mistake. Either family can surge alone: a 403 permission loop or
a Discord 429 storm grows invalid, while a global pause or an open breaker
grows rejected. Both burn the same IP-invalid budget toward the 5,000/600 s
breaker trip and Discord's 10,000-invalid-requests/10-minute IP ban, so this
pages while the budget still has headroom. The first valid sample only stores
a baseline; a backwards counter re-baselines only that family to the new
value and contributes no growth for the window, while the other family's
growth is still judged — the invalid counter steps back by one when a 429
tally is retracted for body-learned shared scope, which is not a restart. A
full reset (both counters back) re-baselines both quietly rather than
firing; a below-threshold window resolves. Missing, malformed or failed
scrapes hold a firing page.

First response: scope the surge before touching the bot. Read both counters
plus `two_bot_rest_guard_invalid_requests_in_window` via the authorized
`/ops/metrics` scrape: invalid-heavy means the bot's requests are being
rejected at headers (check the hot route on
`two_bot_rest_requests_total{route,result}` and correlate with
[REST 429](#alert-rest-429)/[REST 5xx](#alert-rest-5xx) — a 429-heavy surge
with a firing 429 rule is a Discord-side storm, while a 403-heavy surge with
no 429 page is a permission loop or a bad request shape from a fresh deploy);
rejected-heavy means local refusals (correlate with the [breaker
ticket](#alert-rest-admission-breaker-open) and the [token
page](#alert-rest-token-invalid) — a surge beside a firing token page is
token-driven and follows the token procedure; a surge during a Discord
global pause is pause-driven backpressure, see the global-pause notes in the
breaker section). Confirm no deploy is in
progress; a fresh deploy shipping a bad token or a hot invalid route fills the
window fast. Contain through the actual writer's verified control
([containment](#containment-kill-switches-and-feature-flags)): stop the
offending workload and fix the missing permission or caller bug rather than
repeatedly restarting to erase the budget. Do not hammer Discord, replay
uncertain writes, or invent a breaker-reset command — there is no manual
reset.

Escalate when the surge repeats across windows after containment, when invalid
requests keep arriving with no suspect deploy (a caller may be minting bad
requests in a loop, or an abuse pattern is driving the bot's sends), or when
the surge coincides with missed gateway events or 5xx alerts suggesting a
wider Discord or network incident.

#### Alert: REST global responses stuck

Global-pause response bodies stayed unresolved
(`two_bot_rest_guard_pending_global_responses` nonzero) for three consecutive
valid keepalive samples. A pending global body still blocks admission when its
provisional remaining time is zero, so three stuck samples mean REST sends
have been refused for minutes while no deadline elapses to release them. Body
reads carry a 5-second deadline and expiry drops their accounting, so this is
24-36x past the expected drain: the bodies are stuck, not slow. A valid zero
sample proves the bodies drained and resolves; missing, malformed or failed
samples break the streak but hold a firing page.

The paused/remaining gauges deliberately have no rule of their own: a live
global deadline is healthy Discord backpressure — pausing sends during a
Discord global cooldown is the guard working as designed, not a fault — and
the remaining countdown derives from a monotonic deadline that decreases
every scrape and cannot wedge high, so no threshold on it could distinguish a
long Discord cooldown from a fault. Only the pathological case, bodies that
never resolve, pages here.

First response: confirm the stall via the authorized `/ops/metrics` scrape —
pending nonzero across samples while the pause countdown drains or sits at
zero — then read the container logs for `discord_global_pause` lines and
global-body expiry errors around the first stuck sample. Let any live Discord
deadline elapse; do not bypass the guard or resubmit concurrent copies of the
same action. Do not restart the container to "clear" the counter: a
replacement resets the baseline without proving the bodies resolved, and the
rule re-fires on three fresh stuck samples if the fault survives.

Escalate when the stall survives a live-deadline expiry, when it coincides
with missed gateway events or 5xx alerts, or when pending keeps climbing with
no `discord_global_pause` line in the logs — the cause is then unexplained
and the fix belongs to the on-call engineer, not another redeploy.

#### Alert: DB pool

The SQLx pool sat at its maximum with zero idle connections for three
consecutive keepalive samples. This is pool exhaustion, a proxy for DB trouble;
recorded storage failures have a separate [DB-error alert](#alert-db-errors).
It means every checkout is held — new queries wait rather than fail fast —
not proof that the database itself is down (pool gauges
sample SQLx bookkeeping, not DB reachability).

First response: check the affected provider's status (PlanetScale for
production, Neon for staging) before touching the bot; then look at recent
deploys for a change that could hold checkouts open (new query path, widened
job fan-out, a job whose cadence no longer matches its duration). Compare
against the scrape window — a short burst that self-clears across the next
samples is not exhaustion. Do not run SQL probes against staging or
production, add grants, or restart the container to "free" the pool; a
replacement restarts the shard without fixing a leak.

Escalate when the streak persists after the suspect deploy is identified,
when exhaustion coincides with gateway `starting`/`down` or job-failure
alerts, or when the affected provider's dashboard shows trouble on the
deployed branch — the fix then belongs to the dependency owner, not a
redeploy.

#### Alert: DB errors

Three or more storage-layer failures arrived between two keepalive samples.
`two_bot_db_errors_total` counts send-admission SQL failures (`op="admission"`)
and voice actor load failures (`op="other"`); other stores adopt the counter
incrementally, so this is not a database-wide total. Unlike the pool rule
above, this is errors, not pressure: queries already failed, they are not
merely waiting. A counter reset (process restart) skips the window rather
than firing, and a slow trickle below threshold stays silent. Failed jobs
may also surface through `job_consecutive_failures`; that rule does not
cover every storage path.

First response: check the affected provider's status (PlanetScale for
production, Neon for staging) before touching the bot; then correlate with
the `op` label and recent deploys (a new query path or migration can explain
a fresh error burst). For voice actor load failures, read `voice actor load
failed; retrying with capped backoff` in container stdout (or `voice actor
load failed; credential refused, not retrying` when the store refused the
bot credential). Transient failures retry inside the actor task (30 s
per-attempt deadline, 1 s backoff doubling to a 60 s cap), so the guild
recovers on its own once storage heals; a credential refusal never retries
and needs the credential fixed first. Every failure increments the
counter, but that warn is throttled process-wide to once per 300 s; its
absence in a short window does not prove the actor loaded. Do not run SQL probes against staging or
production, add grants, or restart the container to "clear" the errors;
a replacement restarts the shard without fixing the failing storage path.

Escalate when the burst repeats across windows, when it coincides with pool
saturation or job-failure alerts, or when the affected provider's dashboard
shows trouble on the deployed branch — the fix then belongs to the dependency
owner.

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
`two_bot_voice_orphans_total`, `two_bot_voice_vote_kick_total`). Fires when failed operations exceed 5% of
room operations with at least 10 operations in the window (the T3 room
create/move/delete budget in
[voice-cutover-rollback-triggers.md](voice-cutover-rollback-triggers.md)),
when any new dead-letter (a queue write that exhausted 10 attempts) or
orphan (an untracked creator-channel orphan needing manual deletion)
appears — so a low-volume stranded-member failure still pages — or when any
new vote-kick enforcement failure (`discord_error`, `gave_up` or
`permission_missing` on `two_bot_voice_vote_kick_total`) appears, so a
vote-kick path that always fails enforcement pages even while room
operations look healthy. Any outcome
other than `success` (`category_full`, `discord`, `persistence`,
`cancelled`) counts as a failure: the join did not place a room, or the
move/delete did not complete. `refused` (a room cap or create burst, per
member or per guild) is admission policy working as designed, so it never
pages. No installed rule watches it: read it from
`two_bot_voice_operations_total{outcome="refused"}`, and if joins stop placing
rooms while it climbs, look for a stuck cap (an unsettled reservation holding a
slot). Benign vote-kick outcomes never page either: refusals such as
`cooldown` are admission policy, and the remaining enforcement codes
(`connect_denied_and_disconnected`, `connect_denied_target_absent`,
`skipped_room_gone`, `skipped_target_protected`) are successful or
already-terminal writes. A counter reset (process restart) skips the window rather than firing,
and a slow trickle below threshold stays silent. A missing vote-kick family
(older containers never emitted it) or a malformed sample holds a firing
alert instead of resolving it.

First response: scope the failing operation from the `op`/`outcome` labels
on `two_bot_voice_operations_total` and the `outcome` label on
`two_bot_voice_vote_kick_total` via the authorized `/ops/metrics`
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

#### Alert: voice join fast burn

Creator joins are burning the 99.5%-moved-within-5 s error budget faster than
14.4x in both the last hour and the last 5 minutes
(`two_bot_voice_joins_received_total` minus benign `duplicate` refusals in
`two_bot_voice_join_refused_total`, against
`two_bot_voice_join_to_move_seconds_bucket{le="5"}`). Every non-duplicate
refusal, failed move and slow (>5 s) move is burn. Each window needs at least
10 genuine joins and two bad moves; one event alone cannot page or ticket.
Receive counters lag one scrape so an in-flight move can complete. After startup
or restart, a window stays inactive until its full history is available; a
counter reset rebuilds the ring instead of firing.

First response: confirm the spike is joins, not moves, via the authorized
`/ops/metrics` scrape — compare `joins_received` growth against
`voice_join_refused` by `reason` and `voice_operations{op="move"}` outcomes.
`halted` means the actor is parked (paged by
[voice worker halted](#alert-voice-worker-halted));
`unknown_creator`/`stale_ticket` point at creator-map or ticket generation
drift; `discord`/`persistence` move outcomes point at REST or storage. Then
read the container logs for `voice_event="voice_join_received"` refusal lines
and `voice_event="voice_join"` terminal lines with their stage durations.

The burn rules cannot see joins that never reach the actor. If members stop
being moved while `joins_received` stays flat and no refusal lines appear,
read `two_bot_voice_live_not_ready_seconds` (paged past 120 s by
[voice live not ready](#alert-voice-live-not-ready)): above 0 means the
guild's live view is not ready on a connected gateway, so joins are dropped
before a ticket. Every dispatch retries the cached snapshot
(`two_bot_voice_live_self_heal_total` counts the repairs, ticketed by
[voice live self-heal](#alert-voice-live-self-heal)); a value that keeps
climbing past 120 s needs a restart (fresh IDENTIFY + GUILD_CREATE) and a bug
report with the logs around the last gateway close.

Escalate when the burn persists across hours (the slow-burn ticket below will
already be open), when `halted` refusals dominate, or when the stall matches a
deploy — the fix belongs to the on-call engineer, not another redeploy.

#### Alert: voice join slow burn

The same creator-join budget is burning faster than 6x in both the last 6
hours and the last 30 minutes. This is the ticket-severity companion to the
fast-burn page above: a moderate but sustained degradation — a slow leak of
refusals or p95 creep past 5 s — that never spikes hard enough to page.
Same signals, same first response, same escalation, lower urgency. Do not
close it as "fast burn resolved": the fast window can clear while the slow
grind continues.

The first firing also files one tracking issue by itself. Every five minutes
the `slow-burn-tracker` workflow scrapes staging `/ops/metrics`, replays the
checked-in slow-burn verdict over its cached ring and reconciles the
`slow-burn` label: no open card means file one, an open card stays untouched
(a refresh comment lands at most every six hours, so a refire never
duplicates the card), and a cleared burn resolves and closes it. An unknown
verdict — a failed scrape or evaluation, any scrape refused by the Worker's
strict `parseAlertScrape` validator (including corrupt non-join series), or
slow windows without full history — never closes the card. A refused scrape
is held: no issue is filed, refreshed or closed, and neither ring nor coverage
cache is rewritten. A lost, malformed or over-1 MiB ring rebuilds instead of
firing; an over-cap coverage sidecar discards old stamps and seeds fresh
tracking. Cache reads are byte-bounded before buffering. Driver and issue-list
JSON answers are limited to 1 MiB of UTF-8 before JSON decoding; malformed,
undecodable, excessively nested or over-cap answers fail with a
fixed-vocabulary error and an evidence receipt, never a raw parser traceback.
Issue rows require positive
integer numbers (not booleans) before choosing the oldest open issue. The five-minute cadence is load-bearing for
the six-hour baseline: GitHub runs `*/5` best-effort, so when the newest
ring entry is 600 s or older the run emits a `::warning::` instead of
reading green-with-unknown. Entry age alone cannot prove coverage — sub-600 s
spacings can also leave the baseline window empty — so the tracker records
the most recent covered moment beside the ring, seeds the tracking start on
the first live unknown run when it never covered, and warns once the slow
windows go ~6.5 h uncovered: while coverage is gone genuine burns go unfiled
and open cards stay unclosed. Live runs read the existing `staging`
environment bearer `STAGING_METRICS_SCRAPE_TOKEN` for `/ops/metrics`, bound
with `deployment: false` so the five-minute scrape adds no staging deployment
record (the main-only branch policy still applies); without the bearer or
the staging Worker URL the run fails closed with an error naming the unset
binding. Prove the path by dispatching the workflow with the fire and
clear fixtures: a separate `fixture` job, which binds no environment and
holds no scrape bearer, runs the real verdict and the real issue calls under
the `slow-burn-test` label without touching the live ring.

#### Alert: voice creator-map stale

The voice actor re-merges the creator map from the store every 5 minutes;
`two_bot_voice_creator_map_age_seconds` counts from the last *successful*
merge (a failed read never resets it, and the stall watchdog keeps it
climbing while the actor is hung). This alert fires when the age passes two
refresh intervals (600 s) with `two_bot_voice_creator_map_generation` above
0, meaning merges stopped landing and creator joins are served from a stale
map: new or changed creators are invisible (`unknown_creator` refusals) and
removed creators keep working. Generation 0 (no merge yet: boot, or voice
parked) never fires, and a container old enough to lack the gauges holds the
previous state instead of resolving it.

First response: confirm via the authorized `/ops/metrics` scrape that the
generation is flat while the age climbs, then look for the cause one layer
down — store read failures (`db_errors`, pool saturation) or a halted actor
(paged by [voice worker halted](#alert-voice-worker-halted)). Read the
container logs for
`voice_join_received` refusal lines: a rising `unknown_creator` share with a
flat generation is this alert becoming member-visible.

Escalate when the store cause is not self-clearing, when `unknown_creator`
refusals dominate, or when the flat generation survives a restart — the fix
belongs to the on-call engineer, not another redeploy. Guarded remediation
recommends alongside this alert: the bot's own reload first, then a same-SHA
takeover card if the alert survives it (see
[auto-remediation](#auto-remediation-guarded-stall-and-crash-loop-response)).

#### Alert: voice worker halted

`two_bot_voice_worker_halted` reads 1: at least one guild actor is halted by
a store CredentialRefused or Discord 401, so that guild's voice processing is
parked (`halted` join refusals; the join-budget burn rules follow if joins
keep arriving). Pages on a single valid 1 reading — the latch is published
every actor tick from live state, never a transient blip — and a valid 0
clears at once. A container old enough to lack the gauge holds the previous
state instead of resolving it.

First response: confirm the latch via the authorized `/ops/metrics` scrape,
then tell the two halt causes apart in the container logs:
`voice halt re-probe skipped: Discord credential latch set` (debug) means a
Discord 401 — only a token fix and a restart clear it;
`voice halt re-probe: store still refusing` (debug) means the 60 s store
re-probe has not recovered a CredentialRefused halt yet, while
`voice worker halt cleared: store credentials healthy` (info) means it just
did. While the latch holds, find the dark guild via
`voice_event="voice_join_received"` `halted` refusal lines (they carry the
refusing channel).

Escalate when the latch survives the re-probe interval, when the logs name
the Discord latch, or when burn alerts join the page — the fix belongs to
the on-call engineer, not another redeploy.

#### Alert: voice live self-heal

`two_bot_voice_live_self_heal_total` increased between samples: a guild's
live voice view was left not ready on a connected gateway, so creator joins
in that guild were being dropped, and the next dispatch republished the view
from the gateway cache. Ticket, not page: each increase is one covered
fault, already repaired. The first sample is baseline-only and restarts
re-baseline; a missing family on older containers is no window, never a
verdict.

First response: read the container logs for
`voice live view was not ready on a connected gateway; republished from cache`
(warn, one line per repair) to confirm the repair and its guild; then check
`two_bot_voice_live_not_ready_seconds` — 0 means the view is ready again
or the gateway is down, so confirm gateway readiness via the `/readyz`
breakdown before treating 0 as recovery; still climbing means the
self-heal is covering a view that will not stay ready
(see [voice live not ready](#alert-voice-live-not-ready)).

Escalate when repairs repeat for the same guild, or when a repair coincides
with gateway close/resume churn — the fix belongs to the on-call engineer,
not another redeploy.

#### Alert: voice live not ready

`two_bot_voice_live_not_ready_seconds` past 120 s: a guild's live voice view
has been not ready for over two minutes while the gateway is connected
(READY or RESUMED handled), so creator joins in that guild are dropped
without an accept or refusal line
(`two_bot_voice_joins_received_total` stays flat). Promotes the host
watchdog's >120 s bound into the standard pipeline. Pages on a single valid
reading past the bound — the age itself proves continuous not-ready — and a
valid reading at or below 120 s clears at once. Older containers without the
gauge hold the previous state. A hung dispatch does not freeze the published
age: the watchdog sibling reads the shared live view directly, so a hang
still pages once the age passes 120 s.

First response: confirm via the authorized `/ops/metrics` scrape that the
age keeps climbing while the gateway is connected; check
`two_bot_voice_live_self_heal_total` — repairs without recovery mean every
dispatch retries the cached snapshot and the view will not stay ready. If
the age is frozen while joins drop, check the container build first —
current builds keep the age climbing through a hang; on an older build,
read the container logs for
`voice worker stuck: queued actions with no progress` (error): a hang
predating the not-ready view explains the frozen age. Then restart for a
fresh IDENTIFY + GUILD_CREATE and file a bug report with the logs around
the last gateway close (the republished-from-cache lines plus the close
frame).

Escalate when the age survives the restart, or when several guilds go
not-ready together (gateway-side, not one guild's view) — the fix belongs
to the on-call engineer, not another redeploy.

Guarded remediation recommends alongside this alert, in its own scope so it
cannot spend the creator-map scope's window: `dispatch-retry` first (the bot's
own dispatch republishes a not-ready view from the gateway cache on every voice
frame, so a self-healing view can clear with no step at all), then one
same-SHA-takeover Operator card if the alert survives that window (see
[auto-remediation](#auto-remediation-guarded-stall-and-crash-loop-response)).
The restart above is that takeover card, not an ad-hoc host action.

#### Alert: voice queue stall

`two_bot_voice_queue_oldest_age_seconds` past 120 s: the oldest queued (not
in-flight) voice action has waited over two minutes — a guild's queue is
draining unacceptably slowly. The bound matches the in-process stuck horizon
(`watchdog_stuck_after`: queued actions with no completed dispatch turn for
120 s bump `two_bot_voice_watchdog_stucks_total`). A hung dispatch does not
freeze publication: the watchdog sibling recomputes the queue age from the
snapshot's first-enqueue stamp on its own clock, so a hang that starts
below the bound still tickets once the age passes 120 s. Ticket, not page:
a slow-but-healthy
drain can legitimately age past the bound under burst load, and
member-visible join degradation already pages through the join-budget burn
rules. A valid reading at or below 120 s clears at once; older containers
hold the previous state. Queue depth
(`two_bot_voice_queue_pending{lane}`) is intentionally unwatched: depth
without age is not a failure signal — read it as the depth complement, not
the verdict.

First response: confirm via the authorized `/ops/metrics` scrape that the
age keeps climbing; read `two_bot_voice_queue_pending{lane}` for which lane
is deep and `two_bot_voice_worker_halted` — a halted actor explains a
climbing age at once (follow [voice worker halted](#alert-voice-worker-halted),
not this ticket). Then read the container logs for
`voice worker stuck: queued actions with no progress` (error, the in-process
verdict) and `voice dispatch turn exceeded observe bound` (warn, a slow turn
that still completed). When this ticket is absent but joins stall, that
stuck line is still the fastest verdict: on current builds the ticket
follows once the climbing age passes the bound, while on an older container
a frozen age below the bound means the stuck line is the only signal —
treat it as the verdict and escalate on it.

Escalate when the age climbs while the latch reads 0 and no stuck line
appears, or when burn alerts join the ticket — the fix belongs to the
on-call engineer, not another redeploy.

#### Alert: error budget fast burn

The REST error budget (99.9% success over the 48h watch, `docs/error-budget-alerts.md`)
is burning faster than 12x in both the last 15 minutes and the last 2 minutes
(`two_bot_rest_requests_total` eligible requests against `result="5xx"` plus
`result="transport"` errors; `4xx`/`429` are caller or rate-limit outcomes, not
spend). Each window needs at least 10 eligible requests and two budget errors;
one bad minute alone never pages. After startup or restart, a window stays
inactive until its full history is available; a counter reset rebuilds the
window history instead of firing. A scrape gap never resolves a firing page by
itself: a window without coverage holds the existing state, and only full
coverage below threshold resolves. Window edges land on whole keepalive scrapes,
so a burn sustained exactly at the policy rate reads up to ~1% low and the
comparison carries that allowance instead of flapping.

First response: confirm the spike is server-side via the authorized
`/ops/metrics` scrape — compare `5xx` growth against `transport` growth by
`route`. `transport` points at timeouts and cancellations before response
headers (check job timeout classes); `5xx` concentrated on one route after a
deploy points at a broken request shape from the fresh build (check the page's
build SHA and the recent-deploy rollback one-pager); `5xx` across routes
points at the Discord API (check the Discord status page). Then read the
container logs for the failing route's error lines.

Escalate when the burn persists across windows (the budget exhausts in 4 hours
at full fast burn), when half the 48h budget is spent, or when the spike
matches a deploy — the fix belongs to the on-call engineer, not another
redeploy.

#### Alert: error budget slow burn

The same REST error budget is burning faster than 3x in both the last 2 hours
and the last 15 minutes. This is the ticket-severity companion to the
fast-burn page above: a moderate but sustained degradation — a slow leak of
`5xx` or `transport` errors — that never spikes hard enough to page. Each
window needs at least 10 eligible requests and two budget errors; quiet
traffic and one bad event stay silent. After startup the ticket waits for a
full 2 h of history; a counter reset (process restart) rebuilds the ring
instead of firing and holds a firing ticket instead of resolving it, so a
deploy never posts a spurious RESOLVED. A scrape gap or missing series
holds a firing ticket, and a covered quiet or below-threshold 15 min window
resolves it even while the 2 h window is still unknown. Window edges land
on whole
keepalive scrapes, so the comparison carries the same ~1% allowance as the
fast rule.

First response: same signals as the fast burn — compare `5xx` against
`transport` growth by `route` via the authorized `/ops/metrics` scrape, then
read the container logs for the failing route. Triage within the watch
shift; do not close it as "fast burn resolved": the fast window can clear
while the slow grind continues.

Escalate to a page when half the 48h budget is spent, when the slow burn
persists across two consecutive 2 h windows, or when the leak matches a
deploy — the fix belongs to the on-call engineer, not another redeploy.

#### Alert: gateway missed events

`two_bot_gateway_missed_events_total` increased between two keepalive
samples. These are dispatches Discord assigned but this process never
received (sequence gaps inside one session). Any increase fails the
zero-missed-events acceptance: the session continued, but part of the
event stream is gone and RESUME does not replay it. The first sample after
monitoring arms only stores the baseline and never fires, and a counter
reset (process restart) skips the window rather than firing.

First response: read the paired `two_bot_gateway_disconnects_total` counter
via the authorized `/ops/metrics` scrape. A missed-events increase without
a disconnect increase is an unexplained same-session sequence gap, not
proof that instrumentation predates the gap or that the process restarted.
A rise next to disconnects warrants investigating transport loss with
sequence gaps. Re-baseline both scrapes after a confirmed restart.
Correlate with recent
deploys (a fresh deploy restarts the process and resets the counter) and
the container logs for `gateway reconnect failed; Twilight will retry` and
`gateway ready; checkpoint committed`. Do not restart the container to
"clear" the counter; a replacement resets the baseline without recovering
the missed dispatches.

Escalate when the increase repeats across windows, when it coincides with
unpaired disconnects (no later RESUME or fresh READY), or when missed
events rise with no disconnect at all — the gap is then unexplained and
the fix belongs to the on-call engineer, not another redeploy.

#### Alert: gateway unpaired disconnect

`two_bot_gateway_disconnects_total` grew between keepalive samples and the
recoveries (`two_bot_gateway_resumes_total` plus fresh READY from
`two_bot_gateway_events_total{event="READY"}`) have not caught up for 3
consecutive completed samples. Every disconnect should pair with a later
RESUMED or fresh READY; an unpaired one means the gateway never came back.
This is page severity (SEV-1 in the
[watch escalation path](watch-escalation-path.md)).

The bound is 3 samples. Samples arrive about every 60 s and a normal reconnect
resumes within seconds. A RESUMED that lands before the next sample or the one
after clears the unpaired count while the streak is at most 2, one short of
the bound. The third consecutive unpaired sample comes about two to three
minutes after the disconnect, so a reconnect that resumes within two samples
never pages and a gateway that stays down does.

A disconnect is outstanding while the recovery counters have not grown since
the disconnect growth started. Any growth in RESUMED plus fresh READY clears
the outstanding loss at once, even when the same window also added
disconnects: the disconnect series counts every failed Twilight retry, close
frame, op 9 invalid session and cold-resume IDENTIFY, so one recovered outage
routinely adds several disconnects against a single recovery, and subtracting
the growths would page forever on the leftover (see the pairing warning in
[cutover dashboard queries](cutover-dashboard-queries.md)). The alert fires
when a loss stays outstanding for 3 consecutive samples and resolves on the
first sample with recovery growth. The first sample only stores the baseline. A
missing, duplicated, mislabeled, or invalid counter sample changes nothing and
holds the current state. Gateway counters must be nonnegative safe integers.
A counter that went backwards, or a change in the validated
`two_bot_process_incarnation` identity from the same metrics scrape, means the
process restarted: that sample is not a window, the streak restarts, and a
firing alert holds rather than clearing. The next valid flat sample resolves;
new disconnect growth still opens an outstanding loss. This prevents a healthy
replacement's equal recovery sum and higher disconnect total from being
misread as a same-process loss. A ready `/readyz` gateway component alone never
suppresses an outstanding disconnect. The firing key is the constant
`gateway_unpaired_disconnect`.

Process identity consists of four fixed `part` series (`0` through `3`), each an
unsigned 32-bit integer, forming one nonzero random process-lifetime value.
It is independent of image revision, wall clock, PID and DO lifetime. All four
rows must be present, uniquely labeled, and valid before identity can be used;
malformed identity fails the scrape and holds the state. Older containers may
omit the entire family. Legacy stored state without a valid identity adopts
its first identity without discarding an outstanding loss: adoption is not
proof of restart. It still needs real recovery growth, a counter decrease, or
a later observed identity change to retire that loss. After an attributed
baseline, missing identity holds the pairing state rather than comparing
unattributed counters. Do not delete or reset alert storage to work around an
ambiguous legacy latch.

A process boundary is attribution evidence, not proof of gateway health or a
clear remediation baseline. The authorized rollout still needs serving-image
verification, continuous captures and a full successfully evaluated 30-minute
clear/reset sequence with expired cooldown. Neither this rule change nor its
fixture tests arm a drill or authorize a restart.

First response: scrape `/ops/metrics` as described above and compare the
disconnect and recovery counters with the previous scrape. Then read the
container logs for `gateway reconnect failed; Twilight will retry` (the gateway
is still retrying) and `gateway ready; checkpoint committed` (a recovery). If
the logs show no `gateway ready` after the last disconnect, the gateway is still
down: escalate to the on-call engineer through the watch escalation path. Do not
restart the container to clear the alert. A restart re-baselines the counters
without showing that the gateway came back, so confirm `gateway ready` in the
new process's logs first. Guarded remediation recommends alongside this alert:
a same-SHA takeover card, then a CEO page if two takeovers fail (see
[auto-remediation](#auto-remediation-guarded-stall-and-crash-loop-response)).
A reload step is skipped here: no in-process reload can fix a gateway that
never came back.

#### Alert: gateway not ready

The scraped `/readyz` gateway component stayed non-ready (`starting` or
`down`) for 3 consecutive valid samples. Unlike
[`gateway_unpaired_disconnect`](#alert-gateway-unpaired-disconnect), this rule
needs no disconnect edge: it catches a gateway stuck starting (Armed,
reconnecting but never connecting), parked (Unconfigured, so the shard never
starts) or draining, where `two_bot_gateway_disconnects_total` never grows
and the pairing rule stays silent. This is page severity (SEV-1 in the
[watch escalation path](watch-escalation-path.md)).

The bound is 3 samples. Samples arrive about every 60 s, so the third
consecutive non-ready sample comes about two to three minutes after the
gateway stopped being ready. The bound matches the sibling unpaired rule, so
both gateway pages tell one story, and at the default `UNREADY_ALERT_FAILURES`
threshold it never fires sooner than the generic
`container_unready_alert` (about two minutes of failed `/readyz` probes): the
generic page fires first and this rule attributes the outage to the gateway
with its own runbook, an order of magnitude inside the 15-minute
acknowledgement window. (A raised `UNREADY_ALERT_FAILURES` override can invert
that order; do not rely on generic-first with an override.) At the default
threshold, any outage long enough to trip this rule already
tripped the generic unready page a sample earlier, so it adds no new
false-page class for slow boots; with an override it can page first, so a
slow boot can page here with no prior generic page.

A non-ready component is read from the `components` breakdown of the
keepalive `/readyz` probe, not from `gateway_failure.failed_step`: the
failure field is absent until a failure is recorded, so a shard stuck
starting with no recorded failure has no step to read, while the component
entry always names the gateway state. Only `ready` (Connected) counts as
ready; `starting` and `down` both extend the streak, and unknown vocabulary
holds rather than guessing. The first valid sample only stores the baseline.
A failed probe, an unparsable body, or a failed `/metrics` scrape changes
nothing and holds the current state, so the rule needs consecutive ticks
where both the probe parsed and the scrape succeeded. A counter reset
(process restart) re-baselines the streak and holds a firing non-ready or
invalid alert rather than clearing it. A `ready` sample clears at once,
even across a restart.
The firing key is the constant `gateway_not_ready`.

A parked gateway (Unconfigured, for example a missing `DISCORD_TOKEN`) pages
here too, exactly as it already trips the generic unready page: confirm the
expected token configuration before touching the bot. The staging
rollback-drill injection reports this exact shape on purpose, so a drill that
holds the injection for three consecutive samples trips this rule by design;
treat a page inside a known drill window as the drill working — the drill
gate rolls back on the 503 regardless.

First response: scrape `/ops/metrics` as described above and confirm the
disconnect and recovery counters are flat (flat counters with this page mean
the gateway never started or never came back without an edge, not an
unpaired loss). Then read `/readyz` for the full component breakdown: a
`database` or `token_invalid` down alongside a ready gateway is not this
alert's territory. Read the container logs for `gateway reconnect failed;
Twilight will retry` (still retrying) and `gateway ready; checkpoint
committed` (a recovery). If the logs show no `gateway ready`, the gateway is
still down: escalate to the on-call engineer through the watch escalation
path. Do not restart the container to clear the alert. A restart re-baselines
the streak without showing that the gateway came back, so confirm `gateway
ready` in the new process's logs first.

#### Alert: gateway latency dead

`two_bot_gateway_latency_seconds` carried no heartbeat-ACK evidence for 3
consecutive valid samples. The gauge reports the last heartbeat round-trip
from Twilight's completed ACK sample, or NaN until the first ACK lands on
the current connection (every reconnect resets it to unknown, so a stale ACK
never looks like latency on the new connection). Either unknown (NaN) or an
unchanged finite value across samples means no fresh ACK arrived: healthy
connections ACK every heartbeat interval (~41 s), well inside the ~60 s
scrape cadence, so three evidenceless samples (~2-3 min, the sibling gateway
bound) mean the gateway is dead — no heartbeats answered, no events flowing.
Any fresh evidence (a finite change, or a finite value after unknown) clears
at once. A clean boot (Connected within ~1 min, first ACK within another
interval) never reaches the third sample. The first valid sample only stores
the baseline and never fires; malformed rows (duplicated, labeled, negative,
infinite) and missing rows hold the previous state, and a failed scrape
re-baselines the streak while holding a firing page. This is page severity
(SEV-1 in the [watch escalation path](watch-escalation-path.md)). The firing
key is the constant `gateway_latency_dead`.

Read this page beside [`gateway_not_ready`](#alert-gateway-not-ready): both
firing together mean the gateway never connected (follow the not-ready
runbook first), while this page alone with readyz still Connected means a
half-dead connection — the shard looks up but answers nothing, the case no
other gateway rule can see. A parked gateway (Unconfigured, for example a
missing `DISCORD_TOKEN`) pages here too on its permanent NaN, exactly as it
already trips the generic unready page: confirm the expected token
configuration before touching the bot.

First response: scrape `/ops/metrics` as described above and read the
latency row plus the disconnect, reconnect, resume and READY counters. NaN
with flat counters means the gateway never connected; an unchanged finite
value with flat counters means ACKs stopped mid-connection; NaN beside
climbing disconnects means the transport is churning (see the flapping
runbook below). Then read `/readyz` for the component breakdown and the
container logs for `gateway ready; checkpoint committed` (a recovery) and
`gateway reconnect failed; Twilight will retry` (still retrying). Do not
restart the container to clear the page. A restart re-baselines the streak
without showing that ACKs flow again, so confirm fresh latency movement in
the new process's scrapes first.

Escalate when the page survives a confirmed reconnect (fresh HELLO with no
ACK after it points at the Discord-side session, not our transport), when it
coincides with missed-events or unpaired-disconnect pages (the outage is
losing traffic, not just heartbeats), or when the gateway flaps between
Connected and down without ever holding — the fix then belongs to the
on-call engineer, not another redeploy.

#### Alert: gateway reconnect flapping

`two_bot_gateway_reconnects_total` rose in 3 consecutive keepalive samples.
The counter counts successful transport reconnections (new HELLOs after the
first HELLO in the running loop — not failed dial attempts), so one blip is
a clean recovery and only a sustained rise means the session cannot stay up:
the transport drops and re-establishes faster than the session can settle.
Single reconnects stay log-only per the watch checklist; this page needs
three consecutive rising windows (~2-3 min, the sibling gateway bound). A
flat window resolves at once, a backwards counter (process restart)
re-baselines, the first valid sample only stores the baseline and never
fires, and missing or malformed samples hold a firing page without
manufacturing evidence. This is page severity (SEV-1 in the [watch
escalation path](watch-escalation-path.md)). The firing key is the constant
`gateway_reconnect_flapping`.

A flapping transport usually pairs with climbing disconnects; what it does
not by itself prove is traffic loss — a session can bounce between clean
resumes without dropping a dispatch. Any sequence gap the flapping causes
pages immediately and separately via
[`gateway_missed_events`](#alert-gateway-missed-events), so read the two
together: flapping alone is transport instability, flapping plus missed
events is an outage losing traffic.

First response: scrape `/ops/metrics` as described above and read the
reconnect, disconnect, resume and missed-events counters over the last few
windows. Reconnects climbing with matching resumes means Discord keeps
asking for fresh sessions (check Discord status before touching the bot);
reconnects climbing with flat resumes and climbing disconnects means our
dial or resume path is failing (read the container logs for `gateway
reconnect failed; Twilight will retry` and any TLS/DNS/connector errors).
Confirm no deploy is in progress: a rollout restarts the process and resets
the baseline, but a crash loop looks like flapping with a side of restarts —
correlate container restart counts before concluding the transport is at
fault. Do not restart the container to "clear" the counter; a replacement
resets the baseline without steadying the session.

Escalate when flapping persists past one full heartbeat interval of stable
deploys, when it coincides with missed-events, unpaired-disconnect or
latency-dead pages, or when resumes flatline while reconnects climb (the
session re-establishes but never resumes — the fix belongs to the on-call
engineer, not another redeploy).

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

#### Alert: receiver refusals

Refused `two_bot_internal_actions_total` outcomes rose for one family in
3 consecutive keepalive samples. Every outcome other than `executed`
(`auth_failure`, `unknown_key`, `clock_skew`, `nonce_replay`,
`rate_limit`, `unknown_action`, `action_disabled`, `malformed_body`,
`conflict`, `upstream`, `internal`) counts as a refusal: the signed
website-action receiver saw the request and refused it. The streak (not
a single window) pages, because a receiver-abuse or refusal storm stays
quiet through the burn math while one forged probe must not. The firing
key names its family (`receiver_refusals:moderation`). The first sample
after monitoring arms only stores the baseline and never fires, and a
counter reset (process restart) clears that family's streak rather than
firing. Family `other` holds every pre-auth refusal (bad signature,
unknown key, clock skew, nonce replay map through `ActionLabel::Unknown`),
so a lone `receiver_refusals:other` streak points at forged traffic
before a wired-family misconfiguration.

First response: scope the refusing family from the `family`/`outcome`
labels on `two_bot_internal_actions_total` via the authorized
`/ops/metrics` scrape; confirm no deploy is in progress (a fresh deploy
restarts the process and resets the counter); then read the container logs
for the matching `internal action refused` warn lines (sampled summaries
with `kind`/`class`/`key`/`action`). A single historic refusal with no
growth never re-pages. Do not retry-loop a signed request with a new
nonce, rotate keys, or restart the container to "clear" the counter; a
replacement resets the baseline without stopping the abusive caller.

Escalate when refusals persist across windows after the suspect deploy or
caller is identified, when they coincide with 429 or DB-error alerts, or
when `executed` traffic for the same family collapses while refusals rise
— the receiver may be refusing legitimate work and the fix belongs to the
on-call engineer, not another redeploy.

#### Alert: checkpoint failures

`two_bot_gateway_checkpoint_failures_total` increased between two keepalive
samples. Every checkpoint failure stops the dispatch worker, so there is no
benign singleton: a single increase pages. The first sample after monitoring
arms only stores the baseline and never fires. A running-gateway failure
lingers 75 s before it exits (startup failures still linger 15 s), which
leaves about 3 s of margin over one 60 s keepalive tick plus two 6 s probe
timeouts for a scrape to see the increase; the post-restart reset sample
resolves the alert rather than firing.

First response: if the failing process is still up (within the linger), read
the `stage` label on `two_bot_gateway_checkpoint_failures_total` via the
authorized `/ops/metrics` scrape — `pre_commit` means the commit was skipped
after a funnel, leveling or acknowledgement failure, while `commit` means the
durable store write itself failed. The Worker log line
(`container_gateway_failure`) and `/readyz` `gateway_failure` carry the phase,
class, failing step and SQLSTATE code plus the `release`/`worker_version` tags
— both checkpoint stages map to `gateway_runtime_failed` — but never the
checkpoint `stage`. Once the process has restarted, the stage is not
recoverable: the counter resets to zero. Do not wait for the stage then; check
the `gateway` component on `/readyz` and the Neon status for the staging branch
instead: a `commit` failure next to DB-error or pool alerts points at the
database, not the gateway. Do not restart manually: the process exits on its own
after
the linger and the supervisor restarts from the committed checkpoint — verify
the replacement is healthy per the
[restart semantics](#restart-semantics-durable-resume-not-full-state-recovery).
Do not restart to "clear" the counter, since a replacement resets the
baseline without recovering the uncommitted checkpoint.

Escalate when the increase repeats across windows, when `commit` failures
coincide with DB-error or pool-saturation alerts, or when `pre_commit`
failures rise with no failing dependency in the logs — the cause is then
unexplained and the fix belongs to the on-call engineer, not another
redeploy.

#### Alert: dispatch drops

`two_bot_dispatch_drops_total{lane}` grew in three consecutive completed
keepalive sample windows after a baseline. `dispatch_drops:<lane>` is a
**ticket, not a page**, for each of the fixed `messages`, `interactions`,
`registry`, `privileged`, `busy` and `reactions` lanes. A large single burst
stays silent. Dispatch admission refused local work; this is **not proof of
gateway packet loss** or a sequence gap. The `reactions` counter also includes
per-member fairness refusals while lane slots remain free, so it can point at
a hot member rather than an undersized lane.

First response: triage within the watch shift. Read the affected lane's counter
through the existing authorized `/ops/metrics` path, correlate the window with
`command dispatch saturated` or `command dispatch fairness drop` sampled logs,
and compare gateway readiness, missed-events and REST/DB alerts separately.
The counter measures every refusal but logs are sampled once per 60 s per
runtime; a quiet log is not proof of quiet counters. Record only fixed lane
names and sanitized counts, never member/channel IDs, payloads or scrape tokens.

Flat samples clear that lane's streak and resolve its ticket. A backwards
counter in any observed lane signals a process reset: all valid lanes re-baseline
with zero streaks and resolve, even if another lane's new count overtook its old
value. Missing lanes still retain active tickets. Missing/invalid series or a
failed scrape break the streak and discard
the baseline, **not** the active ticket; the first valid sample after a gap is
only a baseline, ongoing growth retains the ticket, and a subsequent valid
flat/reset window proves recovery. A new DO field or legacy state starts
unbaselined. Counter-only monitoring cannot detect a restart whose replacement
counter already exceeds the old value; correlate deployment evidence rather
than claiming that case is proven quiet.

Do not enlarge lanes, relax fairness limits, replay dropped reactions or restart
the bot to clear a counter. This alert adds neither automatic ticket-system
integration nor paging escalation: it labels the existing transition delivery
as `(ticket)` and supplies this runbook. Persist sanitized incident evidence and
route a continuing dispatch problem to the on-call engineer through the normal
triage path. No new webhook, production activation or live load test is authorized.

#### Alert: handler duration saturated

The `two_bot_handler_duration_seconds` dispatch tail crossed into seconds:
p99 past 1 s in 3 consecutive keepalive samples. The histogram covers
nonduplicate dispatch parse, pipeline and durable commit; the rule reads p99
exactly from the bucket deltas (the share of completed dispatches the 1 s
bucket did not absorb exceeds 1%) and additionally requires at least 10
completions (quiet windows stay silent) and at least 2 slow ones (one slow
dispatch stays silent). The 1 s line is the first seconds-scale bucket — a
healthy p99 sits in the millisecond buckets — so a sustained crossing means
second-scale tail work (slow commits, lock contention, a dependency with a
long tail), not ordinary jitter. Any judged-quiet window or counter reset
(process restart) clears at once; the first valid sample only stores the
baseline and never fires; missing or malformed samples drop the baseline and
hold a firing ticket without manufacturing evidence. This is a **ticket**,
not a page: the pipeline still works, only slowly. The firing key is the
constant `handler_duration_saturated`.

This ticket is the saturation backstop, not an SLO: routine quantile
movement stays log-only in the [watch signal queries](watch-signal-queries.md)
compare-across-windows pack, which the ticket points at for diagnosis.
Frozen buckets (zero completions between samples) resolve this ticket — a
dispatcher completing nothing is a different failure from a slow one, so a
resolve beside flat `_count` growth means look at [dispatch
drops](#alert-dispatch-drops), [gateway missed
events](#alert-gateway-missed-events) and consumer lag instead of closing
the incident.

First response: scrape `/ops/metrics` as described above and read the
handler bucket deltas for the firing windows — which `le` buckets absorbed
the growth locates the slowdown (growth only above 1 s is tail-only; growth
shifting down through the millisecond buckets is the whole pipeline
slowing). Then run the action-latency quantile queries for the same window
and compare against the prior 24 h, and read the container logs for slow
commit, lock-wait and REST-timeout lines. Do not restart the container to
"clear" the tail; a replacement resets the baseline without draining the
slow dependency, and the next three saturated windows re-ticket.

Escalate when the tail keeps climbing across tickets (the dependency is
degrading, not flapping), when saturation coincides with dispatch drops,
pool saturation or DB errors (the slowdown found its cause — follow that
signal), or when p99 crosses 5 s (the top finite bucket: dispatches now take
longer than a keepalive tick, and the fix belongs to the on-call engineer,
not another redeploy).

#### Alert: prefix trigger verdict refusals

`two_bot_gateway_prefix_trigger_refused_total{reason="verdict"}` rose in
three consecutive keepalive samples after a baseline.
`prefix_trigger_verdict_refusals` is a **ticket, not a page**: in-scope
prefix candidates were refused by the automod verdict before any trigger
lookup, so a sustained rise means a verdict outage is refusing every prefix
command — not that members are typing unknown commands. Unmatched content
(no trigger), scope mismatches, bot/webhook messages and disabled
text-command gates never increment this family, and `reason="other"` is
normal refusal volume that never counts toward the streak. A single refusal
or burst stays silent.

First response: triage within the watch shift. Read the `verdict` series
through the existing authorized `/ops/metrics` path and correlate the
window with automod verdict health (contained, timed-out, uninspected,
unrecorded or missing verdicts) and any deploy in progress — a fresh deploy
can briefly refuse while verdicts backfill. Record only the fixed reason
name and sanitized counts, never message content, verdict text or user
labels.

Flat samples and counter resets clear the streak and resolve the ticket.
Missing/invalid series break the streak and discard the baseline, **not**
the active ticket; the first valid sample after a gap is only a baseline,
ongoing growth retains the ticket, and a subsequent valid flat/reset window
proves recovery. A new DO field or legacy state starts unbaselined.

Do not change verdict logic, moderation policy or prefix routing to clear
this alert, and do not replay refused commands. This alert adds neither
automatic ticket-system integration nor paging escalation: it labels the
existing transition delivery as `(ticket)` and supplies this runbook.
Route a continuing verdict outage to the on-call engineer through the
normal triage path. No new webhook, production activation or live load test
is authorized.

#### Alert: community facts drain failures

`two_bot_community_facts_drain_failures_total{reason}` grew in three consecutive
completed keepalive sample windows after a baseline. `community_facts_drain_failures`
is a **ticket, not a page**: gateway dispatch deliberately warns and continues
when the community-facts writer errors or misses its deadline, so scorecard data
loss can otherwise remain log-only while checkpoints look healthy. Growth is
summed over the fixed `error`, `timeout` and `other` reasons. A large single
burst stays silent.

First response: triage within the watch shift. Read the per-reason counter
through the existing authorized `/ops/metrics` path, correlate the window with
`gateway community facts dispatch failed` / `gateway community facts dispatch
timed out` warn logs, and check scorecard coverage separately: the scorecard
fails closed on missing coverage, so a drain-loss window means the scorecard
undercounts rather than overcounts. Record only fixed reason names and
sanitized counts, never member/channel IDs, payloads or scrape tokens.

Flat samples clear the streak and resolve the ticket. A backwards counter in any
fixed reason signals a process reset: all reasons re-baseline with a zero streak
and resolve, even if another reason's new count overtook its old value.
Missing/invalid series or a failed scrape break the streak and discard the
baseline, **not** the active ticket; the first valid sample after a gap is only
a baseline, ongoing growth retains the ticket, and a subsequent valid flat/reset
window proves recovery. A new DO field or legacy state starts unbaselined.
Counter-only monitoring cannot detect a restart whose replacement counter
already exceeds the old value; correlate deployment evidence rather than
claiming that case is proven quiet.

Do not change facts capture, gateway commit semantics, privacy/retention or
scorecard calculations to clear this alert, and do not restart the bot to clear
a counter. This alert adds neither automatic ticket-system integration nor paging
escalation: it labels the existing transition delivery as `(ticket)` and supplies
this runbook. Persist sanitized incident evidence and route a continuing drain
problem to the on-call engineer through the normal triage path. No new webhook,
production activation or live load test is authorized.
#### Alert: EventSub post failed share

`two_bot_eventsub_posts_total{outcome}` failed share across keepalive
samples: failed posts exceed 10% of attempted posts in the window, with at
least 10 attempted posts. `eventsub_post_failed_share` is a **ticket, not a
page**. The denominator is attempted posts, not succeeded plus failed: growth
in `succeeded` from earlier attempts or in `other` never counts as delivery
failure. Announcement traffic is low-volume, so windows below the floor hold
the baseline and accumulate attempts across consecutive valid samples until the
floor is met; only the judged window rebaselines. Low-volume windows (fewer
than 10 attempted) stay quiet at any share, and a judged window at or below
the 10% line stays quiet. The first valid sample
is baseline only, even when every post so far failed. Missing or invalid samples
preserve the active ticket and the baseline; a counter reset (any outcome
moving backwards, including a process restart) rebaselines without firing. Each
each judged window recomputes the share, so only a judged window at or below
the line resolves the ticket. Flat or sub-floor windows hold an active ticket
until that judged window clears it, so a sustained outage stays one ALERT
instead of flapping. Recovery is a healthy judged window with the failed share
back at or below the line; confirm live delivery separately before closing out.
The resolve transition fires exactly once.

First response: read-only diagnosis only. Read the attempted/failed counters
through the existing authorized `/ops/metrics` path and correlate the window
with `eventsub_receiver` log lines; do not re-post announcements, touch the
durable ledger, retry sends, or reconcile subscriptions from this alert. No
broadcaster, body, token or free-text label ever reaches the alert. For delivery
shape and subscription health, follow the existing [stream funnel](stream-funnel.md#transport-webhook)
transport/webhook section and the [EventSub operator runbook](eventsub-runbook.md)
(refusal matrix, challenge verification, dark-unless-enabled behavior) through
their authorized read paths. Record only fixed outcome names and sanitized
counts, never broadcaster IDs, tokens, bodies or scrape secrets.

Do not post announcements, write ledger rows, retry deliveries, or reconcile
subscriptions to clear this ticket. This alert adds neither posting, ledger,
retry, nor reconciliation behavior: it labels the existing transition delivery
as `(ticket)` and supplies this runbook. Route a continuing delivery failure to
the on-call engineer through the normal triage path. No new webhook, production
activation or live load test is authorized.

#### Alert: event-store consumer lag

`two_bot_event_store_consumer_lag_seconds` stayed above 60 s (one keepalive
window) in three consecutive valid keepalive samples. `event_store_consumer_lag`
is a **ticket, not a page**: only lag above the 60 s threshold counts
(fractional durations count), while samples at or below it — including `0`
(unobserved) — never fire and a single transient spike stays quiet. Only a valid
sample at or below 60 s resolves and rebaselines; missing or invalid samples
(absent family, duplicate, labeled, non-finite, negative) and failed scrapes
break the streak but preserve a firing ticket. A process restart reads `0`, so
restarts rebaseline the same way.

First response: triage within the watch shift through the existing read-only
paths. Read `two_bot_event_store_consumer_lag_seconds` via the authorized
`/ops/metrics` scrape and compare gateway readiness, missed-events and REST/DB
alerts separately; a lag ticket with no gateway or store error is consumer
pressure, not proof of capture loss. Diagnose the consumer with read-only
checks only: recent deploy evidence, Worker `metrics_alert` log lines for the
firing and recovery transitions, and the staging read-only verify query in the
[event-store wiring note](event-store.md#staging-verify-steps-read-only).
Record only the sanitized lag value and window, never message, channel, user,
content or scrape tokens.

Do not change the consumer, the store schema, retention or erasure, replay
capture rows, or restart the bot to clear the gauge. This alert reuses the
existing transition delivery as `(ticket)` and supplies this runbook. Persist
sanitized incident evidence and route a continuing lag to the on-call engineer
through the normal triage path. No new webhook, production activation or live
load test is authorized.

#### Alert: metrics scrape lost

The Container DO failed the container-internal `/metrics` scrape on 3 consecutive scheduled keepalive ticks. This is a **ticket, not a page**: the observer itself is blind, so every metric-rule alert is missing evidence while this fires. One failed scrape stays quiet and three consecutive failures ticket exactly once; further failures do not re-ticket while the incident fires.

First response: read the scrape through the existing authorized `/ops/metrics` path, check container `/readyz` and Worker/container logs for `two-bot metrics scrape failed`. Timeouts (6 s deadline), non-OK status, oversized bodies (64 KiB cap) and invalid exposition all count as failure; the deadline and cap are unchanged by this alert. Notifications and logs carry only this runbook link and build context, never response bodies.

A lost scrape never resolves any metric-rule incident: missing evidence is not recovery evidence, so previously firing alerts stay firing through the loss, and the existing dispatch-drop interruption still applies. The first complete valid scrape after the incident resolves only the scrape-loss ticket; metric rules re-evaluate on that same scrape and resolve only on their own evidence. A valid scrape before the third failure silently resets the streak with no resolve.

#### Alert: audit delivery halt

`audit_delivery_halt` is a **ticket, not a page**: the label-free
`two_bot_audit_delivery_halt` gauge stayed `1` for three consecutive valid
keepalive samples. At the default 60 s cadence the first-to-third span is
about two minutes, so an intentional short hold cleared before that span stays
quiet. This is a sample bound, not a maintenance-window authorization or a
wall-clock guarantee at a different keepalive cadence. A deliberate longer
hold still tickets once; acknowledge it through normal incident tracking.

First response: establish whether the hold is intended before attempting to
resume delivery. Correlate the ticket's build/log links with the authorized
maintenance record and the fixed `audit_mirror_halt_engaged` /
`audit_mirror_halt_disengaged` events. A halted sweep intentionally claims
nothing but still records job success every 30 s. Fresh
`audit_retry` success with gauge `1` means a persistent hold, not a wedged
retry loop; do not restart it to clear the ticket. An undocumented hold or one
past its intended end needs the authorized operator to diagnose it this watch
shift. Check DB/pool errors and job failures separately.

An `audit_retry_halt_unreadable` warning is **not a new halt reading**: the
Rust process retains its previous gauge. Its supervisor reports a failed
`audit_retry` attempt. While `two_bot_job_consecutive_failures{job="audit_retry"}`
is nonzero (including other failed audit sweeps), this rule neither fires nor
recovers. Missing, duplicate, labeled or non-boolean halt readings and failed
scrapes also break the pending streak while retaining an active ticket. A valid
`0` with a healthy audit sweep resolves once; a fresh `1` needs a new run of
three samples after recovery. DO reconstruction preserves the streak and active
state; older stored state starts with zero streak. A process restart is not
proof that the persistent switch was cleared: the process's initial zero gauge
cannot resolve a ticket until `audit_retry` has a nonzero last-success timestamp.
Confirm a healthy readable sweep.

Unhalt only through the existing durable store contract, after the authorized
operator confirms the hold is no longer needed: `delivery_halt()` reads its
presence and `disengage_halt()` clears it under the store's existing fence.
See the [audit control boundary](#containment-kill-switches-and-feature-flags) and
[audit store protocol](audit-store.md#crash-and-halt-protocol-for-the-service-slice).
There is **no supported binary audit-halt/unhalt command or operator HTTP
endpoint** here; do not invent one, issue ad hoc SQL, clear pending/quarantined
rows, or reset accepted-send evidence. If that execution path is unavailable,
route the exact request to the authorized operator rather than changing policy.
Verify the next healthy sweep reports `0` and the normal drain resumes; clearing
the alert alone is not delivery evidence.

Only the fixed rule key, ticket classification and public runbook link enter
transition messages. Keep audit content, actor IDs, halt reason text and
free-text labels out of evidence. The existing persist-before-notify path and
forwarding gate are reused; this adds no external ticket-system integration,
new control, retention/delivery change or production activation.

#### Alert: eventsub reconcile failed

`two_bot_eventsub_reconcile_passes_total{outcome="failed"}` rose between two
keepalive samples. The Rust process records exactly one outcome per completed
reconcile pass that reaches Helix (`healthy`, `pending` or `failed`); parked or
refused startup passes never reach Helix and record no sample. A failed pass
means a reconcile action failed or a healthy subscription is still missing,
including listing failures — the detailed per-site error counters
(`two_bot_eventsub_reconcile_errors_total`) add detail only and never multiply
this alert. This is a **ticket, not a page**.

Trigger: the first valid sample only stores a baseline and never fires. A later
positive failed-pass delta fires once, however many internal error counters
moved in the same window; a failed delta beside a healthy delta still fires.
Healthy-only or pending-only growth never fires. Recovery: a flat failed counter
never recovers, and a verification-pending pass never recovers — only a later
healthy-pass increase with no failed-pass increase resolves, once. A backwards
counter in any of the three outcomes means the process restarted: that sample
re-baselines, firing when the new process already counted a failed pass (the
startup pass runs before the first scrape, so a failed boot pass is a
post-restart failure, not a fabricated delta), and otherwise holding a firing
ticket instead of fabricating a RESOLVED. Missing or invalid samples (absent family on older containers,
malformed values, extra labels, duplicates) and failed scrapes hold the firing
state and keep the previous baseline; the next valid failed delta still fires
and the next valid healthy increase still recovers.

First response: read-only diagnosis only — no Helix request and no
reconciliation behavior change. Run the read-only reconcile queries in
[eventsub-subscription-health-queries.md](eventsub-subscription-health-queries.md)
(Q2 last reconcile outcome plus the dashboard fields it drops: `failed`,
`missing`, `prior_failed`, `action`, `error`) via the authorized `/ops/metrics`
scrape and Workers Observability, correlate the window with recent deploys (a
fresh deploy restarts the process and resets the counters), and confirm the
firing key is the fixed `eventsub_reconcile_failed` with no broadcaster,
subscription id, body, token or free-text labels. Forwarding, persistence and
log context are the existing metrics-alert path above (persist before notify,
at most once per transition); turning forwarding off retains monitoring.

Do not retry Helix calls, re-list subscriptions, delete/re-create subscriptions
by hand, or restart the container to "clear" the counter; a replacement resets
the baseline without healing the subscription. This alert adds neither automatic
ticket-system integration nor paging escalation: it labels the existing
transition delivery as `(ticket)` and supplies this runbook. No new webhook,
production activation or live load test is authorized.

#### Alert: EventSub tampered

`two_bot_eventsub_classified_total{kind="tampered"}` rose in 3 consecutive
keepalive samples after a baseline. Only `BadSignature` counts as `tampered`;
every other kind stays silent. The streak (not a single window) pages, because
a sustained rise means an active forgery attempt or a broken secret rotation
while one forged probe must not. The firing key is the bare rule id
(`eventsub_tampered`): no broadcaster, subscription ID, body, token, or
free-text label ever rides on it. The first valid sample only stores the
baseline and never fires, a backwards counter means the process restarted and
rebaselines without firing, missing/invalid scrapes preserve a firing alert
without manufacturing recovery, and a flat valid window resolves.

First response: scope the fixed `kind="tampered"` counter through the existing
authorized `/ops/metrics` path; confirm no deploy is in progress (a fresh
deploy restarts the process and resets the counter); then read the container
logs for the matching `eventsub delivery refused` warn lines (scalar `status`
/ `error_class` only: header values, challenge, body bytes, and query strings
are never logged). A single historic rise with no growth never re-pages.
Forgery versus rotation: a rotation in progress shows 401 `unauthorized`
refusals while the two sides disagree (the running container must already
carry the new secret before old subscriptions are deleted, so not-yet-deleted
subscriptions signed with the old secret fail until the reconciler recreates
them). When no rotation or deploy is underway, treat a sustained rise as
forged traffic. Follow the existing
[secret provisioning and rotation](eventsub-runbook.md#secret-provisioning-and-rotation)
procedure for any credential change; this alert changes no verification, Helix,
or secret handling. Record only the fixed kind and sanitized counts, never
IDs, payloads, or scrape tokens.

Escalate when tampered growth persists across windows after the suspect caller
or rotation is identified, when it coincides with 429, DB-error, or
reconcile-failure alerts, or when `notification` traffic for the same window
collapses while tampered rises — the receiver may be refusing legitimate work
and the fix belongs to the on-call engineer, not another redeploy.

#### Alert: gateway drain skips

`two_bot_gateway_drain_skips_total` rose between valid keepalive samples.
`gateway_drain_skips` is a ticket: auxiliary leveling XP was skipped for
SQLSTATE `42P01` (missing table) or `42501` (missing grant), not a gateway
packet-loss signal. First sample and counter resets only baseline; flat
samples resolve, missing/invalid families retain the baseline without a
window verdict. Older containers may omit the counter.

**First response:** inspect the `gateway leveling dispatch skipped; schema gap`
warn with `drain=leveling`, bounded `cause`, `error_code` and committed-award
count. Never enable raw SQL or error-chain logging. Compare the deployed build
with the migration ledger and effective grants using the existing read-only
inspection procedure. Route a confirmed mismatch to the authorized Operator's
existing schema/ACL repair plan; do not run an ad hoc production migration.

A skip allows checkpoint advance and may lose the failed auxiliary awards;
it promises no automatic replay. Do not invoke retention purge replay for XP.
Non-gap errors and deadlines still hold the cursor and retain their cause;
use the checkpoint-failure procedure instead of bypassing that hold. Dropped
interaction completion and invalid onboarding jobs keep fixed pre-commit
reasons; the missing-ingress-ticket reason remains explicitly post-commit.
After an approved repair, confirm the counter stays flat under normal traffic.
This slice changes rule code only: no alert forwarding or deployment is claimed.

#### Alert: retention replay needed

`two_bot_retention_replay_needed_total` increased between two keepalive
samples. A real guild removal's purge-queue write hit the schema gap (an
unapplied migration or grant), so the removed guild's rows linger until the
Operator replays the insert. Successful queue writes and rejoin-cancel skips
never increment this counter, and the counter resets on process restart, so a
nonzero value means at least one skip since the last restart — the ticket
names the window, not the guild. The first sample after monitoring arms only
stores the baseline and never fires, and a counter reset (process restart)
skips the window rather than firing. This is a **ticket**, not a page: no
member impact and no data loss, but every skip is bounded operator work.

First response: read the error log line `retention purge skipped; schema gap,
operator replay required` over the whole schema-gap window — it carries the
guild id and SQLSTATE (`42P01` missing table, `42501` missing grant) and stays
the source of truth for which guilds to replay. Confirm the migration or ACL
plan is now applied, then replay per the
[retention procedure](privacy.md#retention-and-explicit-exceptions): run the
same `retention_guild_purge_queue` insert the live path runs, but only when no
`GUILD_CREATE` for that guild was acked after the error line — a later
`retention lifecycle skipped; schema gap, nothing to cancel` warn means the
guild rejoined, so skip the insert while the guild is live. Run the default
dry run first and confirm the guild is still gone before executing. Do not
restart the container to "clear" the counter; a replacement resets the
baseline without replaying the purge.

Escalate when skips repeat across windows after the migration lands (the gap
is not the schema), when the error lines name `42501` on rejoins (the ACL
plan is still unapplied and the cursor stays held), or when a skip coincides
with DB-error or pool-saturation alerts suggesting a wider database incident.

#### Alert: voice dispatch timeouts

`two_bot_voice_dispatch_timeouts_total` increased between two keepalive
samples. One actor dispatch turn ran past the 60 s observe bound. The turn ran
to completion and the queue keeps its retries, so a single overrun is
slowness, not loss — but an over-budget turn proves only that the total turn
exceeded 60 s, which bounded awaits, lock contention, scheduling delay, or
downstream latency can also produce, and repeats mean the actor is not
keeping up. The first sample after
monitoring arms only stores the baseline and never fires, and a counter reset
(process restart) skips the window rather than firing. This is a **ticket**,
not a page: one slow turn never pages, while member-visible join degradation
already pages through the join-budget burn rules.

First response: read the warn log line `voice dispatch turn exceeded observe
bound` for the guild and elapsed seconds, plus the queue-side posture from the
authorized `/ops/metrics` scrape (`two_bot_voice_queue_oldest_age_seconds`,
`two_bot_voice_live_not_ready_seconds`). A lone overrun next to a healthy
queue is one slow turn — note it and watch the next windows. Do
not restart the container to "clear" the counter; a replacement resets the
baseline without addressing the slowness.

Escalate when overruns repeat across windows, when they coincide with
watchdog-stuck or queue-stall tickets (the actor is not keeping up), or when
they coincide with sustained join-budget burn — the slowness is then
member-visible and the fix belongs to the on-call engineer, not another
redeploy.

#### Alert: voice watchdog stucks

`two_bot_voice_watchdog_stucks_total` increased between two keepalive samples.
Queued actions saw no completed dispatch turn for the full 120 s stuck window:
the actor had work and made no progress on it. One stuck window can self-clear
on the next window, so this is a **ticket**, not a page — but a stuck window
is never healthy idleness (an empty queue never fires the watchdog), and
member-visible join degradation already pages through the join-budget burn
rules. The first sample after monitoring arms only stores the baseline and
never fires, and a counter reset (process restart) skips the window rather
than firing.

First response: read the error log line `voice worker stuck: queued actions
with no progress` (at most one per stuck window), then check the sibling
posture from the authorized `/ops/metrics` scrape: the halt latch
(`two_bot_voice_worker_halted`), the live-not-ready age, the queue oldest age
and the join-budget burn rules. A stuck window next to a halted latch suggests
the halt rather than the queue — but both signals aggregate across all guild
actors with no guild labels, so co-occurrence alone does not prove the halt
caused this stuck window; confirm attribution in the guild-scoped log lines,
then follow the halted-worker section. A stuck window with a
hung dispatch is the in-process verdict; anything else points at a slow drain.
Do not restart the container to "clear" the counter; a replacement resets the
baseline without draining the queue.

Escalate when stuck windows repeat across windows, when they coincide with
dispatch-timeout tickets (turns overrun and no progress lands), or when they
coincide with sustained join-budget burn — queued members are then waiting
past the stuck horizon and the fix belongs to the on-call engineer, not
another redeploy.

#### Alert: voice halt recoveries

`two_bot_voice_halt_recoveries_total` increased between two keepalive samples.
A store-CredentialRefused halt cleared via the 60 s credential re-probe: the
halted worker read the store under timeout and the latch cleared, so the next
join flows. Discord-401 halts never reach the probe — only a token fix and a
restart clear those — so this counter never counts a token recovery; every
increase is a store fault the automation covered. This is a **ticket**, not a
page: the fault already passed, but the halt refused member joins while
latched, and the store refusal is worth a look — including brief halt/recover
cycles the sampled halt latch never shows. The first sample after monitoring
arms only stores the baseline and never fires, and a counter reset (process
restart) skips the window rather than firing.

First response: read the info log line `voice worker halt cleared: store
credentials healthy` for the guild, plus any `voice halt re-probe: store
still refusing` or `timed out` lines around it from the container logs. A lone
recovery after one refusing probe is a transient store blip — note it and
watch the next windows. Confirm the halt latch reads 0 on the authorized
`/ops/metrics` scrape; a latched 1 next to a recovery means some guild actor is
still halted — possibly the same guild re-halted, possibly another (the latch
aggregates all current guild actors with no guild labels); use the
guild-scoped log lines for attribution. Do not restart
the container to "clear" the counter; a replacement resets the baseline
without explaining the refusal.

Escalate when recoveries repeat across windows (the store credentials flap),
when a recovery coincides with DB-error alerts (the refusal is a wider
database incident), or when the latch stays 1 with no further recovery (the
probe keeps failing and the halt needs operator action, not another redeploy).

#### Alert: REST breaker opens

`two_bot_rest_guard_breaker_opens_total` increased between two keepalive
samples. The 600 s invalid-request window reached threshold (default 5000
observations of 401, 403 or 429 response headers) and the guard started
refusing non-essential REST sends locally. Essential interaction
acknowledgements may bypass this breaker, but never a global pause or fatal
token state. This is a **ticket**, like its sustained-open sibling: a trip on
a bad deploy is routine, and the breaker closes on its own when the window
cools below threshold — closes are the resolution evidence for this ticket,
not a rule of their own. The first sample after monitoring arms only stores
the baseline and never fires, and a counter reset (process restart) skips the
window rather than firing.

First response: read the guard's `discord_breaker_open` log line for the
invalid-request count and threshold, plus `two_bot_rest_guard_invalid_requests_in_window`
and `two_bot_rest_guard_rejected_requests_total` from the authorized
`/ops/metrics` scrape; confirm whether a deploy is in progress (a fresh
deploy shipping a bad token or a hot invalid route fills the window fast).
There is no manual breaker reset: do not restart the container to "clear" it —
a restart only resets the counters and destroys the evidence. As with the
sustained-open ticket, check `two_bot_rest_guard_token_invalid` separately:
ticket resolution on a cooled window is not REST recovery while the fatal
token latch stays set — follow the
[fatal-token procedure](rest-guard.md#fatal-bot-token-and-readiness).

Escalate when trips repeat across windows after the rolling window should have
cleared, when invalid requests keep arriving with no suspect deploy (a caller
may be minting bad requests in a loop), or when the ticket coincides with
missed gateway events or 5xx alerts suggesting a wider Discord or network
incident.

#### Alert: REST global pauses

`two_bot_rest_guard_global_pauses_total` increased between two keepalive
samples. A new Discord global REST pause began: all guarded REST admission
holds until the learned deadline passes. Extensions and overlapping pending
responses never increment this counter, so every increase is a genuinely new
episode. This is a **ticket**, never a page: the pause self-clears at its
deadline and no operator action shortens Discord-side backpressure. Every 429
response, including global 429s, also counts toward the 600 s invalid-request
window and can contribute to opening the breaker — a pause episode next to a
breaker trip is the expected shape of a global-limit storm, not two
independent incidents. The first sample after monitoring arms only stores the
baseline and never fires, and a counter reset (process restart) skips the
window rather than firing.

First response: read the guard's `discord_global_pause` log line for the
wait, plus `two_bot_rest_guard_global_paused`,
`two_bot_rest_guard_global_pause_remaining_seconds` and
`two_bot_rest_guard_pending_global_responses` from the authorized
`/ops/metrics` scrape; confirm whether a deploy is in progress (a fresh
deploy shipping a request storm trips the global limit fast). There is no
manual pause release: do not restart the container to "clear" it — a restart
resets the counters, drops pending response state, and loses the in-memory
deadline; the new process does not retain the pause unless it observes another
global 429. Read the remaining duration together with
`global_paused` and pending responses, not as a standalone readiness signal.

Escalate when new episodes arrive back-to-back across windows with no
Discord-side incident (our own request pattern may be tripping the global
limit), when episodes coincide with breaker trips and sustained 429 rates, or
when a pause never clears past its learned deadline — the hold is then
unexplained and the fix belongs to the on-call engineer, not another redeploy.

#### Alert: feed fetch failures

`two_bot_feed_fetch_errors_total` summed over the twelve fixed `error_class`
values increased between two keepalive samples. A feed poll failed: the
`error_class` names the cause (`transport`, `deadline`, `http_error` for a
completed non-2xx fetch, `resolve`, source-policy refusals, or `other` for
an unknown class), and every failed fetch counts exactly once. Successful
fetches record nothing here, so any increase is a real failure, however
small. The first sample after monitoring arms only stores the baseline and
never fires, and a counter reset (process restart) skips the window rather
than firing. This is a **ticket**, not a page: one dead source or one
transient blip never pages, while a poller failing every window keeps the
alert firing. The transition path sends one ticket when firing starts and
does not re-notify while it remains firing.

First response: read the `error_class` breakdown from the authorized
`/ops/metrics` scrape, then the feed poller warn/error lines for the same
window — they carry the failing source and the attempt detail the
per-outcome (never per-feed) counter omits. A lone `transport` or `deadline`
blip next to healthy polls is one slow origin — note it and watch the next
windows. A stuck `invalid_source` or `non_public_address` class means the
configured source can never succeed: fix or remove the source rather than
watching the ticket repeat. Do not restart the container to "clear" the
counter; a replacement resets the baseline without healing the source.

Escalate when failures repeat across windows on many classes at once (the
poller or the network path is down, not one source), when they coincide with
`feeds` job failures or DB-error alerts suggesting a wider incident, or when
`http_error` persists after the origin recovered — the fetch shape may be
broken and the fix belongs to the on-call engineer, not another redeploy.

#### Alert: EventSub receive gap

`two_bot_eventsub_received_total` exceeded the summed
`two_bot_eventsub_classified_total{kind}` rows for 3 consecutive keepalive
samples. Deliveries are accepted at body intake but never classified: the
receiver counts each delivery at intake and exactly one classification
(`challenge`, `notification`, `revocation`, `tampered`, `ignored` or `other`)
before answering, with no await between the two, so a healthy scrape shows
the two sides equal. One gapped scrape can land mid-delivery; a gap that
persists across three scrapes (~2 min) means intake outruns classification.
This is a **ticket**, not a page: no delivery is refused or lost by the gap
itself, but unclassified deliveries are unanswered Twitch traffic.

First response: read the received counter and the six classified-kind rows
from the authorized `/ops/metrics` scrape and confirm the gap is still open,
then read the container logs for `eventsub delivery refused` and
`eventsub_receiver` lines over the same window — panics or wedged tasks
between intake and classification leave traces there, while a quiet log next
to a steady gap points at the classification path itself. Confirm no deploy
is in progress (a fresh deploy restarts the process and resets both
counters, which closes the gap). Do not restart the container to "clear" the
gap; a replacement resets the counters without explaining the stall.

Escalate when the gap grows across windows (intake keeps outrunning
classification), when it coincides with tampered-signature or reconcile
alerts suggesting receiver trouble, or when classified counts freeze
entirely while received climbs — the classification path is then wedged and
the fix belongs to the on-call engineer, not another redeploy.

#### Alert: EventSub reconcile prior failed

`two_bot_eventsub_reconcile_errors_total{error_class="reconcile_prior_failed"}`
increased between two keepalive samples. A reconcile pass found Twitch-side
subscriptions on our callback already failing and repaired them (delete +
re-create). The repair already landed — this ticket fires even when the pass
converges healthy and the failed-pass rule stays silent — but Twitch failing
our subscriptions means the receiver was erroring (for example,
notification failures from refused or slow deliveries), so the cause is
worth a look. The rule waits for healthy or pending pass growth before
judging the finding: Rust counts the finding before awaiting the repair and
verification, so mid-pass scrapes retain all baselines without firing. A
failed completion consumes the finding silently; the failed-pass ticket
already names it, even when the finding and the outcome land in different
scrapes. The first complete sample only stores the baseline and never fires.
A backwards finding or pass-outcome counter (process restart) re-baselines
all four counters silently; missing or invalid rows preserve the baselines.
This is a **ticket**, not a page: no action is overdue, but every finding is
a receiver-health question. Passes that never run (dark, parked or a wedged
scheduler) record nothing and cannot ticket — this rule watches findings,
not the absence of passes.

First response: read-only diagnosis only — no Helix request and no
reconciliation behavior change. Read the `prior_failed` count from the
authorized `/ops/metrics` scrape, then the `eventsub reconcile found failed
subscriptions` error lines for the window (bounded broadcaster and
prior-failed counts only, never subscription IDs), and correlate with
receiver refusals and tampered-signature traffic over the same window: a
finding next to refused deliveries explains itself. Confirm the following
passes converge healthy. Do not retry Helix calls, re-list subscriptions,
or restart the container to "clear" the counter; a replacement resets the
baseline without answering why Twitch failed the subscriptions.

Escalate when findings repeat across passes (subscriptions keep failing
faster than reconcile repairs them), when they coincide with failed-pass
tickets (the repair itself is failing), or when a finding names
notification failures with no matching receiver refusal — deliveries may be
failing downstream of the ack and the fix belongs to the on-call engineer,
not another redeploy.

#### Alert: automod sanction refused

`two_bot_automod_decisions_total{outcome="sanction_refused"}` increased
between two keepalive samples. An inspection concluded but its
delete-adjacent sanction could not run: the content decision still recorded,
but the ladder step (delete, warn or timeout follow-through) never landed.
The other seven outcomes stay silent — clean, exempt, dry-run and
already-processed verdicts plus landed ladders are policy working as
designed — so this ticket always means enforcement fell short. Unknown
outcomes collapse to `sanction_refused` in Rust, so a ticket can also mean a
new outcome needs classification rather than a failed sanction. The first
sample after monitoring arms only stores the baseline and never fires, and
a counter reset (process restart) skips the window rather than firing. This
is a **ticket**, not a page: one refused sanction never pages, while a
refusal every window means the ladder is broken for a live abuse class.

First response: read the sanction-refused count from the authorized
`/ops/metrics` scrape, then the automod decision warn/error lines for the
window — they carry the fixed refusal reason (missing permission, lost
race with a manual delete, store write refused) without message text or
user IDs. A lone refusal next to landed deletes is one lost race — note it
and watch the next windows. A refusal naming a missing permission needs the
bot role fixed, not the rule. Do not restart the container to "clear" the
counter; a replacement resets the baseline without landing the sanction.

Escalate when refusals repeat across windows on the same reason (the ladder
is systematically broken), when they coincide with Discord-error or
store-error alerts suggesting the sanction path is down, or when the
refusal reason is unfamiliar — a new outcome may be collapsing into the
watched bucket and the classification belongs to the on-call engineer, not
another redeploy.

#### Alert: moderation enforcement failures

`two_bot_moderation_enforcements_total` summed over every verb and the
`discord_error`/`storage_error` outcomes increased between two keepalive
samples. An enforcement never completed: `discord_error` is an uncertain
Discord failure (timeout, unreachable, rate-limited — the mutation may or
may not have landed), and `storage_error` is a persistence failure. Landed
enforcements (`success`), policy refusals proving no mutation (`refused`),
and unknown collapses (`other`) stay silent. The first sample after
monitoring arms only stores the baseline and never fires, and a counter
reset (process restart) skips the window rather than firing. This is a
**ticket**, not a page: one rate-limited kick never pages, while failures
every window mean moderation is not landing.

First response: read the verb/outcome breakdown from the authorized
`/ops/metrics` scrape, then the moderation enforcement error lines for the
window — they carry the fixed failure reason without member IDs or reason
text. `discord_error` first: check the REST 429/5xx rules for the same
window (backpressure explains timeouts and rate-limited writes); `403` or
`404` shapes inside `discord_error` mean a lost permission or a
manually-removed target, not an outage. `storage_error` points at the
database path — check the DB-error and pool rules. Do not retry the
enforcement by hand on an uncertain `discord_error` without checking
whether it landed; do not restart the container to "clear" the counter — a
replacement resets the baseline without completing the enforcement.

Escalate when failures repeat across windows on many verbs at once (the
Discord or store path is down, not one target), when `storage_error`
coincides with DB-error or pool-saturation alerts, or when `discord_error`
persists with no 429/5xx explanation — the request shape may be broken and
the fix belongs to the on-call engineer, not another redeploy.

#### Alert: voice REST stuck

`two_bot_voice_rest_in_flight_age_seconds` read above 60 s for 3 consecutive
keepalive samples. A voice REST attempt has been in flight for over a minute:
every voice REST send wraps its wire attempt in a 10 s timeout with an
in-flight guard that drops on timeout, completion or early return, so a
healthy gauge never reads past ~10 s and a sustained high age means an
attempt neither completed nor dropped its slot. REST begin/drop events,
actor ticks and an independent observer republish the gauge, so the age
keeps advancing even during a hung dispatch. A stuck reading that survives
three scrapes is a wedged attempt, not a slow one. This is a **ticket**, not
a page: one wedged attempt never pages, while member-visible join degradation
already pages through the join-budget burn rules.

First response: read the in-flight age from the authorized `/ops/metrics`
scrape and confirm it is still climbing or pinned high, then check the
sibling posture from the same scrape: the queue oldest age, the live
not-ready age, dispatch-timeout tickets and the join-budget burn rules. A
stuck age next to a healthy queue and quiet burn rules is one wedged wire
attempt — note it and watch the next windows. Do not restart the container
to "clear" the gauge; a replacement drops the slot without explaining the
wedge.

Escalate when the age stays pinned across windows with no dispatch progress
(the REST path is wedged, not one attempt), when it coincides with
dispatch-timeout or watchdog-stuck tickets, or when it coincides with
sustained join-budget burn — queued members are then waiting on the wedged
path and the fix belongs to the on-call engineer, not another redeploy.

#### Alert: voice global backoff capped

`two_bot_voice_global_backoff_capped_total` increased between two keepalive
samples. A voice action hit a Discord global rate-limit wait past
`GLOBAL_BACKOFF_MAX` (30 s) and shed to tick-driven queue backoff
(`RateLimited` for the action queue) instead of parking the guild actor.
The shed is the designed backpressure shape — the actor stays live and the
gauges stay visible — and no operator action shortens Discord-side limits,
which is why this is a **ticket**, never a page. But every cap is a voice
action deferred by a far-future global limit, and a storm of caps points at
our own request pattern tripping the global limit. The first sample after
monitoring arms only stores the baseline and never fires, and a counter
reset (process restart) skips the window rather than firing.

First response: read the `voice global backoff capped; returning RateLimited
for queue backoff` warn lines for the window (bounded wait seconds only),
plus the global-pause and 429 posture from the authorized `/ops/metrics`
scrape; confirm whether a deploy is in progress (a fresh deploy shipping a
request storm trips the global limit fast). A lone cap next to a Discord-side
limit episode is expected backpressure — note it and watch the next
windows. There is no manual backoff release: do not restart the container
to "clear" it — a restart only resets the counter and the new process
re-learns the same global deadline.

Escalate when caps arrive back-to-back across windows with no Discord-side
incident (our own request pattern may be tripping the global limit), when
they coincide with breaker trips and sustained 429 rates, or when caps
coincide with sustained join-budget burn — deferred voice actions are then
member-visible and the fix belongs to the on-call engineer, not another
redeploy.

#### Auto-remediation: guarded stall and crash-loop response

Execution-path decision: the Worker cannot restart containers, so remediation
is recommendation plus guard state, evaluated in `wrangler/src/remediation.ts`
on every keepalive tick. Nothing there mutates the container, the ownership
record, or any deployment. Execution stays inside the existing machine
contract: the bot's own in-process load — the creator-map merge, or the
live-view republish from the gateway cache (no host action) — then one Operator
host-action card per host step for a same-SHA deployment-takeover through the
existing deploy workflow. No new endpoint, credential, spend, or authority.

Each scope owns its phase/failure guard: `voice_creator_map_stale` (merges not
landing) and `voice_live_not_ready` (a deaf live view on a connected gateway)
are different faults with different first steps, so a shared guard would let
either spend the other's cooldown window and failure count. The 30-minute
cooldown below is global and still caps the whole table.

Trigger table (a recommendation fires only while its trigger key fires):

| Scope | Trigger firing key | R1 (first) | R2 (escalation) |
|---|---|---|---|
| `voice-stall` | `voice_creator_map_stale` | `creator-map-reload`: the bot's maintenance tick retries the merge every 5 minutes; no host action | `same-sha-takeover`: one Operator card for a same-SHA takeover via the existing deploy workflow |
| `voice-live` | `voice_live_not_ready` | `dispatch-retry`: the bot's own dispatch republishes a not-ready live view from the gateway cache on every voice frame (`voice_live_self_heal_total` counts the repairs); no host action | `same-sha-takeover`: one Operator card for a same-SHA takeover via the existing deploy workflow |
| `crash-loop` | `gateway_unpaired_disconnect` | skipped: no in-process reload can fix a gateway that never came back | `same-sha-takeover`: one Operator card, same path as above |

Guard state (persisted in DO storage next to the alert state):

- At most one recommendation per 30 minutes across all scopes. Triggers are
  visited in table order and the first eligible one consumes the window, so
  when both scopes fire at once the table order is the priority and the other
  scope waits for the next window.
- A recommendation while a prior one for the same scope did not clear the
  alert counts as one consecutive failure for that scope.
- After 2 consecutive failures the scope goes quiet and the Worker pages the
  CEO card once, on the same best-effort webhook path as the alerts
  themselves: if forwarding is off or delivery fails, the scope still goes
  quiet until the alert stays clear for a full cooldown window, so a suppressed
  scope with no CEO line is a paging gap, not a recovery. No further
  recommendations until the alert stays clear for one cooldown window.
- Recovery needs a sustained clear, not one quiet line. A recommended takeover
  restarts the process, which re-baselines the gateway counters and boots
  `creator_map_generation` at 0, so the next tick goes quiet even when the
  fault survives. A quiet tick therefore starts a per-scope hold-down instead
  of resetting the guard; only a full 30-minute window of silence returns the
  scope to idle. A refire inside the window keeps the failure count, so a
  flapping alert still reaches the two-failure stop. A scope that stays silent
  for the full window with generation pinned at 0 never pages from this guard:
  the stall rule cannot tell parked from wedged at generation 0, so that case
  needs its own boot/store-fault signal (follow-up), not another redeploy.

Operator card shape for `same-sha-takeover`: name the firing key, the build
revision from the alert line, and the deploy workflow run to dispatch at that
same SHA. File a host-action card only from a `same-sha-takeover` line: the
first step (`creator-map-reload` or `dispatch-retry`) is never card authority.
File exactly one
card per host step; act only while the alert is firing. A repeated takeover
line for the same still-firing alert is the escalation retry, not a second
step: file it only after the first card's action completed without clearing
the alert, never while that action is still in flight. When the CEO page
lands, the on-call pages the CEO card and stops filing takeover cards for
that scope until the alert stays clear for one cooldown window.

Known limits of v1: the triggers stay pinned to the two keys in the table
above. A stuck-gateway rule (non-ready `/readyz` gateway component with no
disconnect edge) is not a trigger in v1; wiring it into the crash-loop scope
is follow-up work once that rule lands, not part of this slice. Sustained
`/readyz` red (`container_unready_alert`) and repeated process restarts stay
manual.

#### Staging fault injector drill

Positive-path staging proof uses the staging-only fault injector
(`wrangler/src/staging-fault.ts`), armed solely through the generated
staging deploy config by a manual `deploy-staging` dispatch with
`inject_voice_stall` and/or `inject_unpaired_disconnect` (both default
false; a push can never arm them, and the switches are never committed
vars). While armed, the keepalive tick holds
`two_bot_voice_creator_map_generation` at 1 with age 660 s (past the 600 s
bound, the 2x300 s refresh) and/or advances
`two_bot_gateway_disconnects_total` with no recovery so the unpaired rule
holds an outstanding disconnect; the alert rules and the guard above run
unmodified, so every recommendation still fires only alongside its trigger.
Expected timeline at the 60 s keepalive: the stall alert fires on the first
armed tick, the unpaired alert needs one baseline tick plus three
outstanding ticks (about 4 min). Observe one full 30-minute cooldown window
for R1-to-R2 escalation (`creator-map-reload`, no host action, then one
Operator card for `same-sha-takeover` at the same SHA); a further window
proves the two-failure CEO page and silence. Disarming re-dispatches the
same SHA without the flags: the triggers resolve on their own rules and the
guards reset. Rollback is disarm-by-redeploy; the Worker recommends only, so
nothing executes from this path.

Drill procedure (Operator). The soak needs the armed build undisturbed for
at least 90 minutes (R1, R2 30+ minutes later, STOP 30+ minutes after R2,
disarm, then a 30-minute clear) while merges keep landing. Arm the hold
first so no push redeploys staging mid-soak: without it, the next push
disarms the drill by redeploy.

1. Pick the drill SHA: the full 40-hex main commit staging currently serves
   (the `build_revision` on staging `/readyz`, not a guess). The pin must be
   an ancestor of `origin/main`. Save it once: `DRILL_SHA=<paste the 40-hex
   SHA>` (replace the placeholder before running; the shell would read the
   angle brackets as a redirection).
2. Arm the hold: `gh variable set STAGING_SOAK_HOLD_UNTIL --body
   "2026-10-19T20:00:00Z"` with an ISO-8601 UTC deadline at most 4h out
   (past the planned disarm plus the 30-minute clear).
3. Arm the fault at the pinned SHA: `gh workflow run deploy-staging.yml
   --ref main -f "sha=$DRILL_SHA" -f inject_voice_stall=true` (add
   `-f inject_unpaired_disconnect=true` for the crash-loop scope).
   Explicit arm/disarm dispatches bypass the hold. Error-rate rollback proofs,
   including the daily automated dispatch, honor it and fail without deploying
   while it is armed; do not clear the hold for a proof.
   The arm run stays red by design: the history guard fails it after a green
   deploy and synthetic so the pin never reads as staging history for the
   head. Read the deploy and synthetic steps, not the run color.
4. Run the R1/R2/STOP observation above. Pushes during the window fail fast
   with the deferral message (red, by design: a quiet green would read as
   staging history to the production guard); merges stay unblocked.
5. Disarm at the SAME SHA with no flags: `gh workflow run
   deploy-staging.yml --ref main -f "sha=$DRILL_SHA"`. The disarm run stays
   red for the same reason while the pin sits below the head.
6. Watch the 30-minute clear (triggers resolve, guards reset), then release
   the hold: `gh variable delete STAGING_SOAK_HOLD_UNTIL`. Letting it expire
   works too: expiry fails open and the next push deploys normally.

#### Staging guard receipts (persisted remediation evidence)

Each keepalive tick writes the evaluator guard to DO storage at
`two-bot:remediation`, flushes it with `storage.sync()`, reads it back, and —
in staging only (`TWO_DEPLOY_ENV` exactly `"staging"`) — logs one structured
receipt proving what was persisted. The flush matters: without it the
read-back is served from the DO in-memory cache/write buffer and always
repeats the just-written value. Production emits none. No endpoint,
permission, dump, or storage mutation beyond the existing evaluator write;
thresholds and the state machine are untouched.

Log query (staging Worker logs): filter for the receipt event, then narrow by
decision or firing key. In Cloudflare dashboard open Workers & Pages →
`two-bot-next-staging` → Observability → Logs and query
`event = "remediation_guard_receipt"` (JSON: `{"event":"remediation_guard_receipt"}`).
Narrow with `decision = "recommend"` (R1/R2), `decision = "ceo-page"`
(STOP), `decision = "suppressed-quiet"` (quiet after two failures),
`decision = "held-cooldown"` (inside the 1800 s window),
`decision = "clear-hold"` (sustained-clear hold-down), or `decision = "idle"`
(pre-drill baseline / reset). Correlate a tick with its alert by matching
`firing` to the same tick's `metrics_alert` line and `build_revision` /
`release` / `worker_version` to the keepalive probe.

Capture schema (allowlist only; no tokens, webhooks, or arbitrary storage):

```json
{
  "event": "remediation_guard_receipt",
  "service": "two-bot-next",
  "ok": true,
  "deploy_env": "staging",
  "evaluated_at_seconds": 1760001860,
  "firing": ["voice_creator_map_stale"],
  "decision": "recommend",
  "action": "creator-map-reload",
  "guard": {
    "lastRecommendedAtSeconds": 1760001860,
    "scopes": {
      "voice-stall": { "phase": "reload-recommended", "consecutiveFailures": 0, "suppressed": false, "clearSinceSeconds": 0 },
      "crash-loop": { "phase": "idle", "consecutiveFailures": 0, "suppressed": false, "clearSinceSeconds": 0 }
    }
  },
  "build_revision": "abc1234",
  "release": "abc1234",
  "worker_version": "deployment-A",
  "log_url": "https://developers.cloudflare.com/workers/observability/logs/workers-logs/"
}
```

A storage failure never yields `ok: true`: a failed write keeps the existing
`two-bot remediation guard failed` line with no success receipt, and a failed
flush (`storage.sync()`), failed read-back, or mismatched read-back yields
`ok: false` with `reason` of `readback_failed`, `mismatch`, or
`invalid_persisted` (same correlation fields, no guard). At most one receipt
per 60 s keepalive tick.

Truthful limits: staging runs with `OPS_ALERT_FORWARDING=off`, so the STOP
(`ceo-page`) decision is log-only — there is no external CEO-page delivery to
capture. The full clean-start R1/R2/STOP/clear sequence takes more than 30
minutes (R1 at first firing, R2 one 1800 s window later, STOP another window
later, then a full 1800 s quiet window to reset; typically about 90 minutes),
so a short soak can only prove the prefix it covers. The receipt proves the
guard persisted, not that any remediation executed — the Worker recommends
only. This receipt is application-level evidence through the Worker's own
authorized guard-storage path: a provider storage query can return transport
HTTP 200 / `success=true` while its nested result is still a denial (for
example `access to _cf_KV.key is prohibited: SQLITE_AUTH` with no rows), so
provider HTTP success alone is not guard evidence.

### Event-store capture gaps and lag

Capture gaps remain operator guidance, not a metrics alert: no rule watches
for missing rows, so gaps never fire. The append-only capture table
(`event_store`) only sees what the gateway received, so its gaps follow the
gateway counters, not a separate store alert. A
`two_bot_gateway_missed_events_total` increase between keepalive samples is a
capture gap: those dispatches never arrived, RESUME does not replay them, and
no backfill can reconstruct them. First response is the same as [gateway
missed events](#alert-gateway-missed-events): scrape the authorized
`/ops/metrics` path, compare the disconnect and recovery counters, correlate
recent deploys, and do not restart the container to clear the counter.

Capture lag has a ticket: [event-store consumer
lag](#alert-event-store-consumer-lag) watches the label-free
`two_bot_event_store_consumer_lag_seconds` gauge. Until the periodic
store-read caller lands, the gauge reads `0` (unobserved); the staging
read-only verify query ([event-store wiring
note](event-store.md#staging-verify-steps-read-only)) remains the manual
check. Backfills and repairs against this table are insert-only with fresh
dedup keys: never rewrite history with `UPDATE` or `DELETE`, and never replay
an uncertain Discord effect from a capture row. Full wiring, fence and staging
steps live in the [event-store wiring note](event-store.md).

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
It embeds two migration chains through the SQLx **0.9.0 library** (no
`sqlx-cli`; the pin is asserted against `Cargo.lock`): this crate's
migrations (ledger `public._sqlx_migrations`) and the store migrations from
`two-bot-store` (ledger `public._two_bot_migrations`, the chain the DML-only
gateway needs at boot). Each chain is planned, applied and verified
separately with its own pending list and plan-binding hash. The runner runs
`SET ROLE` in SQLx's per-connection `after_connect`, verifying `current_user`
on every connection. The two modes
use two credentials and two groups, so the plan is physically read-only:
`--plan` reads only `TWO_BOT_STAGING_PLAN_DATABASE_URL` (a login holding only
`two_bot_migrator_ro`, the `staging-migrate-plan` environment secret) and runs
as `two_bot_migrator_ro`; `--apply` reads only
`TWO_BOT_STAGING_MIGRATOR_DATABASE_URL` (the reviewed `staging-migrate-apply`
environment secret) and runs as `two_bot_migrator`. A mode never reads the
other mode's binding, and an absent binding refuses before any connection.
Invocation (secret-free; each URL comes only from its existing binding):

```text
staging-migrate --target staging --plan --source-sha <40hex> --staging-host <host> \
  --staging-database <db> [--staging-branch-id <branch>] \
  --recovery-evidence-ref <ref> --acl-plan-ref <ref> \
  [--expected-pending <ascending,comma-separated versions>]
staging-migrate --target staging --apply <same flags> --expected-pending <list> \
  --expected-store-pending <store list> \
  --plan-manifest-sha256 <64hex> --store-plan-manifest-sha256 <64hex> \
  --plan-run-id <run id> \
  --plan-manifest-path <producing run's downloaded manifest>
```

Both chains (cutover and store) are planned, applied and verified in one
run, each with its own pending list, manifest hash and ledger; `--apply`
refuses unless both lists and both hashes match the reviewed plan.

`--staging-branch-id` pins the PlanetScale branch id (non-secret). It is
required when `--staging-host` ends in `.psdb.cloud` (PlanetScale routes
branches by the binding username's `{role}.{branch_id}` suffix); other hosts
leave it empty.

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
failed rows), the store ledger's `store_ledger_owner` / `store_ledger_counts`,
one `memberships` entry per migrator login (`two_bot_migrator`,
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
like production, either host pin or the binding host is a pooler endpoint,
the binding uses a pooled port (anything but 5432) or a pooler-style `|` username
(session `SET ROLE` and the migrator lock need the direct 5432 endpoint),
a `*.psdb.cloud` pin has no `--staging-branch-id`, the pin is malformed, or the
binding username's branch suffix does not match the pinned branch, the login
cannot assume `two_bot_migrator` (apply) or `two_bot_migrator_ro` (plan), the
plan login also holds `two_bot_migrator`, a reference is missing, `--apply` has no
`--expected-pending` / `--expected-store-pending` or either mismatches,
`--apply` has no `plan_manifest_sha256` / `store_plan_manifest_sha256` /
`plan_run_id` or either hash does not match the recomputed manifest, `--apply`
has no producing-run manifest or that manifest does not carry the bound hashes,
or either ledger has a failed/incomplete
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

### Automatic pre-deploy drift gate in deploy-staging

`deploy-staging` runs the read-only plan above before every deploy: its
`schema-gate` job (in the `staging-migrate-plan` environment, `contents: read`
only) builds `staging-migrate` at the deploy SHA and runs
`scripts/ci/staging_schema_gate.py --binary ./target/debug/staging-migrate
--source-sha <deploy SHA>`. The `deploy` job needs `schema-gate`, so a failed
or skipped gate means no deploy. The gate reads only the plan binding
(`TWO_BOT_STAGING_PLAN_DATABASE_URL`); it has no apply path, no migrator
binding and no role/grant step.

Before connecting, the gate compares the secret binding against the
non-secret expected staging identity in the `staging-migrate-plan`
environment variables `TWO_BOT_STAGING_HOST`, `TWO_BOT_STAGING_DATABASE` and
`TWO_BOT_STAGING_BRANCH_ID` (Operator-owned; never derived from the binding
itself). Host and database must match exactly (host case-insensitively); on
PlanetScale hosts the binding username's `{role}.{branch_id}` suffix must
match the branch pin, and a PlanetScale pin without a branch id refuses.
Pooler hosts/ports and production-like or empty pins refuse with fixed,
secret-free messages. When the three variables are unset the gate fails
closed, so set and verify them in the environment before relying on deploys.

The gate first binds the returned read-only report to the requested SHA and
verified identity (the [evidence contract](#schema-gate-evidence-and-terminal-results)
below applies to both targets). Only then do `pending_before` and
`pending_store_before` both empty print `schema gate clean` and allow the
deploy; either non-empty exits 1 with
`schema drift: staging database is behind source <sha>: pending versions [...]`
— fix it with the manual `staging-migrate` apply dispatch for that source
(reviewed plan, exact `expected_pending` plus `expected_store_pending` when
the store chain is behind). When applying the store chain, the apply creates a
new ledger whose default privileges do not automatically grant the plan login
read access: reapply the full reviewed role plan (`two-bot db roles plan`), run
`verify` and require 0 findings, then re-run `deploy-staging`. The drift output
prints this sequence. For cutover-only apply, re-run `deploy-staging` after
apply. The gate never fixes drift itself. The plan manifest is written to
`staging-schema-gate-manifest.json`
in the job workspace; its `audit.connection_usage` (sessions summed from
`pg_stat_database.numbackends`, readable in full by the plan login) warns —
never fails — when usage is strictly above 80% of the non-superuser ceiling
(`effective_max_connections`, else `max_connections` minus
`superuser_reserved_connections`, else raw `max_connections` for older
manifests, labelled as such). A saturated staging database (logins refused)
therefore shows in the gate log before the deploy hits it.

#### Schema-gate evidence and terminal results

Both schema gates require a canonical lowercase 40-hex `--source-sha` before
running a plan or writing evidence. The existing report must have strict integer
`runner_version=1`, `mode=plan`, the exact `migration_target` (`staging` or
`production`), the exact requested `source_sha`, `role=two_bot_migrator_ro`,
and strict integer zero `applied_count` and `applied_store_count`. No new
schema-version field is used. `target` must contain string `host`, `database`
and `branch_id` fields matching the independently verified, trimmed environment
pins: host and branch case-insensitively, database exactly. The manifest cannot
choose replacement pins. Missing/minimal/contradictory reports refuse.

Both pending arrays are required, strictly increasing, unique positive i64
integers (no booleans, floats or strings). Duplicate JSON fields, non-JSON,
nonfinite numbers and malformed shapes refuse **before** publishing any plan.
Validated clean and drift reports are saved atomically in `--manifest-out`;
refused/error runs publish no new manifest. An output failure leaves prior
evidence untouched; do not consume a pre-existing file as evidence of this run.
The production script's existing guard-validated rollback/live-SHA exemptions
still record `waived` evidence without reading the database; these are not clean
plan attestations or new waiver authority. Valid connection-usage warnings stay
advisory and do not change the verdict.

The **last nonempty stdout line** is a single-line JSON object with `verdict`,
`code` and `next_action`. Earlier human-readable drift/warning lines remain;
parse the terminal object, not those lines. Codes/actions are fixed tokens:

| Code | Verdict / exit | `next_action` | Meaning |
| --- | --- | --- | --- |
| `schema_clean` | `clean` / 0 | `proceed_with_deploy` | Bound read-only report; both chains clean. |
| `schema_drift` | `drift` / 1 | `run_reviewed_migration_then_retry` | Follow the target's reviewed manual migration and role-restore procedure above. |
| `recovery_waived` | `waived` / 0 | `continue_guard_validated_recovery` | Existing production recovery exemption only. |
| `arguments_invalid` | `refused` / 2 | `correct_gate_arguments` | Repair the CLI invocation. |
| `source_sha_invalid` | `refused` / 2 | `supply_canonical_source_sha` | Supply the resolved lowercase full commit SHA. |
| `binding_refused` | `refused` / 2 | `repair_plan_binding_and_pins` | Operator verifies the existing direct read-only binding and independent pins. |
| `manifest_invalid` | `refused` / 2 | `rebuild_reviewed_plan_runner_and_retry` | Runner output is malformed, unsupported or not this read-only request. |
| `plan_process_failed` | `error` / 3 | `inspect_plan_runner_failure_and_retry` | Plan subprocess exited nonzero. |
| `plan_execution_failed` | `error` / 3 | `repair_plan_runner_execution_and_retry` | Plan subprocess could not execute. |
| `plan_timeout` | `error` / 3 | `investigate_plan_timeout_and_retry` | Plan exceeded its 600-second limit. |
| `manifest_write_failed` | `error` / 3 | `repair_evidence_output_and_retry` | Evidence serialization/output publication failed. |
| `gate_internal_error` | `error` / 3 | `inspect_gate_implementation_before_retry` | Unexpected gate fault; no deploy authorization. |

Failures never relay raw child stdout/stderr, rejected values, filesystem paths,
exception text or tracebacks. Investigate within the existing authorized runner
and credential boundaries; do not bypass the gate or retry with another binding.

### Production migration runner

`.github/workflows/production-migrate.yml` (manual, production-only) is the
exact mirror of the staging runner for post-cutover releases that carry a new
migration. It runs the same `staging-migrate` binary with
`--target production` from `crates/cutover/src/bin/staging_migrate.rs`, embeds
the same crate migrations through SQLx **0.9.0**, keeps the same ledger and
`SET ROLE` behavior, and keeps every staging refusal (pooler, `SET ROLE`
membership, `expected_pending`, manifest hash, `plan_run_id` provenance).
Invocation (secret-free; each URL comes only from its existing binding):

```text
staging-migrate --target production --plan --source-sha <40hex> \
  --production-host <host> --production-database <db> \
  [--production-branch-id <branch>] \
  --recovery-evidence-ref <ref> --acl-plan-ref <ref> \
  [--expected-pending <ascending,comma-separated versions>]
staging-migrate --target production --apply <same flags> --expected-pending <list> \
  --expected-store-pending <store list> \
  --plan-manifest-sha256 <64hex> --store-plan-manifest-sha256 <64hex> \
  --plan-run-id <run id> \
  --plan-manifest-path <producing run's downloaded manifest>
```

`--production-branch-id` pins the PlanetScale branch id (non-secret). It is
required when `--production-host` ends in `.psdb.cloud` (PlanetScale routes
branches by the binding username's `{role}.{branch_id}` suffix, while every
branch shares `postgres` as the database name); other hosts leave it empty.

`--target` is explicit with no default: a run without it refuses, a staging
run never reads a production pin or binding, and a production run never reads
a staging pin or binding. Production reads only
`TWO_BOT_PRODUCTION_PLAN_DATABASE_URL` (plan, read-only
`two_bot_migrator_ro`) and `TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL` (apply,
`two_bot_migrator`); staging keeps its own pair. The production target drops
the `prod`-substring refusal (the production host and `two_bot` database are
production-like by construction) and instead refuses any staging host pin
(fixture hosts, any `staging` label, any Neon endpoint) at both validation and
the binding-host check, so a mistaken production pin aimed at staging still
fails closed before any DDL. The binding-host check additionally refuses any
non-`5432` port and any pooled (`|…`) login on production, so a binding
copied from the app's pooled connection string still fails closed (pooler
`SET ROLE` and the migrator lock need the direct endpoint). A `*.psdb.cloud`
pin without `--production-branch-id`, a malformed pin, or a binding whose
username branch suffix does not match the pin refuses the same way, in both
gates. The manifest carries `migration_target`
(`staging` or `production`); the claim publisher requires the matching value.

The workflow mirrors the staging shape with the `production-migrate-` prefix:
three jobs (`plan`, `claim`, `apply`), environments
`production-migrate-plan` (no reviewer, plan only) and
`production-migrate-apply` (required reviewer, main-only), the same pinned
actions, routed runner, pipefail shell, and artifact names
(`production-migrate-manifest`, `production-migrate-apply-claim`, 14-day
retention, stored ZIP entries). The `claim` job runs
`scripts/ci/production_migrate_claim.py`, which binds the same projection hash
but requires the production workflow path, environment, and
`PRODUCTION_HOST`/`PRODUCTION_DATABASE`/`PRODUCTION_BRANCH_ID` pins (the
claim refuses a PlanetScale plan without the pinned branch id, or a request
pin that does not match the plan). Both environments
and both secrets must exist before dispatch; the host provisions them after
this change merges. The cutover itself does not need this path: the fresh
`two_bot` bootstrap uses the tested provisioner flow.

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
Do not add `--yes` or override warnings. Wrangler 4.152 updates Durable Object
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

**Staging drill.** The manual Worker-only option of `staging-rollback-drill`
(dispatch from `main`, input `target_version`) runs this procedure end to end in the protected
`staging` environment: fence, one unforced Cloudflare deployment of the target
with `code_update_strategy: immediate`, epoch-checked takeover, a 2 s
`/readyz` + `/health` poll, then the same sequence back to the original version.
Pick the target from the deployment list: a version that already served staging
traffic, not the serving one, compatible with the current schema (the script
refuses an unknown or never-deployed id). The job summary and evidence file hold
the fence, rollback, takeover and first-ready times, the time-to-ready against
the workflow's 60 s budget, probe counts, the container image digest and
instance counts. This workflow interval is not an outage-start recovery
measurement and cannot prove B2's under-60-second recovery criterion. An empty
`target_version` instead selects the [daily automatic rollback proof](production-deploy.md#daily-staging-proof),
which also runs on a daily schedule and verifies both Worker and image restoration.
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
on `main` with the staging container image overridden to the digest-pinned
registry reference (every other relative image path is absolutized, because
the generated file lives outside the wrangler directory and wrangler
resolves config-relative paths against it).

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
There is no settings hot-reload/admin endpoint to recommend; the audit halt
handle for disposable agent-testdb fixtures is the `audit_switch` operator
binary ([usage](audit-store.md#operator-handle)). No halt command for a
deployed staging/production store is provided. See the
[incident playbooks](#incident-playbooks) for persistent
ownership containment, actual degradation and stop boundaries.

For unexpected writes: first identify the actual writer (legacy bot, next image,
other automation). Preserve evidence and use the authorized deployment/containment
procedure for that writer. A legacy flag or a SQL row is **not** a verified
next-runtime kill switch. Do not issue SQL against live/staging services, switch
tokens, or redeploy with an unreviewed wiring change during this docs procedure.

| Control | Ported contract | Current operational boundary |
|---|---|---|
| Audit kill switch | Durable `audit_kill_switch`, singleton `id=1`; `engage_halt(actor_id)` / `disengage_halt()` library methods. Halt blocks queue discovery/claim/preparation, not fact recording or an already prepared/in-flight send. | Store and delivery sweep exist (`crates/bot/src/website_jobs.rs:294` registers `audit_runtime`; `crates/bot/src/audit_runtime.rs:141-159` installs the `audit_retry` sweep). Operator handle for disposable agent-testdb fixtures is the `audit_switch` binary ([usage](audit-store.md#operator-handle)); no command in the bot binary itself, and no halt command for a deployed staging/production store. Durable DB errors propagate; legacy pure-model read-fail-open is **not** send authorization. See [audit store](audit-store.md). |
| Automod | Exact `TWO_AUTOMOD=1` enables config. Dry-run by default; exact `TWO_AUTOMOD_ENFORCE=1` selects enforce. | Forwarded and composed into the gateway when activation/configuration permit. Requires valid Owen/protected-role configuration; enforcement can sanction. `TWO_AUTOMOD=1` requests MESSAGE_CONTENT intent. Flags alone do not override identity/permission gates or stop the bot. Filter/effect semantics, repeat expiry, corpus care and triage: [automod operator guide](automod-operator-guide.md). |
| Moderation | Exact `TWO_MODERATION=1`; enabled config requires `TWO_OWEN_USER_ID`; protected-role policy applies. | Forwarded; channel handlers `/purge`, `/slowmode`, `/lockdown`, `/unlock` are registered and subject to activation and permission checks. Do not infer every member-moderation builtin executes. No config toggle demonstrated as an emergency stop. |
| Automations/announcements/text | `TWO_AUTOMATIONS=1`, `TWO_ANNOUNCEMENTS=1`; text needs automations **and** `TWO_TEXT_COMMANDS=1`. | Per-feature gates, not operational containment. Verify the affected deployed handler; registry publishing or a periodic job alone does not prove a specific action is active. |
| Onboarding | `TWO_ONBOARDING_MODE=legacy|session|anchor`, default legacy; `TWO_ONBOARDING_DRY_RUN=1`. | Mode/dry-run contracts are feature-scoped, not bot-wide stop controls. Verify the affected handler and staging evidence; a catalogue value alone is not a runtime activation or reload receipt. |
| Community scorecard | `TWO_COMMUNITY_SCORECARD=1`; recommendations on unless `TWO_COMMUNITY_RECOMMENDATIONS=0`. | Conditionally registered supervised job; durable retry budget and completion rules apply. A successful/no-op tick is not fresh publication proof. See the [database playbook](#neon-or-hyperdrive-outage). |
| Internal actions | Direct-container support and optional gates are described in the [receiver contract](internal-actions-receiver.md#implementation-and-deployment-boundary), not proof of shipped Worker capabilities. | Staging-only and default dark under both ingress gates. The Worker does not forward role keys or optional receiver capability gates; adding secrets cannot enable them. Use the [single wiring table](internal-actions-receiver.md#container-wiring) and [intake/forwarding recovery contract](internal-actions-receiver.md#intake-forwarding-and-correlation). Production has no ingress. |
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

### Scheduled delivery uncertainty

Before a scheduled POST, the store arms `delivery_uncertain` under the current
claim token. Timeout/transport loss, an unusable receipt, or failed completion
with unconfirmed cleanup retains that fence and occurrence nonce. Restart and
lease expiry never authorize another POST or advance a one-shot/recurring run.
Only proven no-effect refusal or confirmed cleanup permits retry. Admission
completion must be confirmed too; an accepted DELETE or missing id is not proof
of cleanup.

Diagnostic: `scheduled_delivery_uncertain`; safe next action:
`inspect_channel_before_cancel_or_replace`. The attempt audit uses
`delivery_uncertain` or `persistence_failed_uncertain`, never clean success.
Inspect the target channel and the schedule's audit first. An already-authorized
administrator may then cancel with `/schedule-remove`, or intentionally replace
the definition through the existing schedule workflow. Replacement is a **new
send** and can duplicate a surviving old message. Do not clear the fence, force
retry, or infer absence from missing/unreadable/bounded channel history. This
slice performs no automatic history scan, even for a positive receipt.

Migration 0142 is additive and conservatively fences existing nonce/claim rows;
NULL fences fail closed. Restoring a pre-0142 archive re-fences its pending
nonce/claim rows the same way; current archives restore the recorded fence
faithfully. Rollout must stop old blind-replay writers first. Do
not downgrade to a binary that ignores the fence; retain it when reverting code.
This is source behavior, not evidence of a migration or live release.

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
| DB unreachable / missing schema or grants | `DATABASE_URL` is required. Gateway connect/hydration/checkpoint failure can exit the process; its SQL error is withheld. Gateway and lazy jobs are DML-only; runtime will not provision schema. Job failures may coexist with ready gateway state, while the live `database` ping can independently make `/readyz` 503. Follow the [database outage playbook](#neon-or-hyperdrive-outage); repair the named dependency through its owner, not another credential, SQL probe or widened grant. |
| HTTP 200 health but persistent 503 ready | Listener works; inspect `gateway`, `database` and `token_invalid` state and sanitized logs. A database failure or rejected-token latch need not be a gateway transport outage. Never soften readiness or count the scaffold-era deploy gate as recovery. |
| Reconnect / RESUME refused | Follow [restart semantics](#restart-semantics-durable-resume-not-full-state-recovery); 4007/4009 force fresh IDENTIFY. Preserve the durable checkpoint, don't hand-edit sequence or start another shard. |
| Discord REST 429 / suspected breaker | Separate token-wide durable admission, executor-local pacing, process-wide global pause/invalid-request breaker, and the private announcement governor. Refusal can precede HTTP; retry bounds vary by action. There is no manual reset endpoint. Do not hammer Discord, replay uncertain moderation writes or restart/delete state to clear a hold. Identify the actual writer and use verified containment; see the [Discord playbook](#discord-gateway-or-api-outage). |
| Channel moderation lane stuck `in_progress` after an ambiguous write | No automatic retry/expiry. Quiesce original workers, establish old REST settlement, and read actual Discord overwrites/slowmode before using the inspection-first, explicitly confirmed operator CLI. It preserves recovery and audits the prior claim. See [channel lane reconciliation](channel-lane-reconciliation.md); never release a lane while a delayed unlock can still write. |
| Website event action stuck `needs_reconciliation` after an ambiguous create | No automatic retry: re-submitting under a new key can make a second event. Read the actual guild scheduled events in Discord, then resolve the exact intent with the inspection-first, explicitly confirmed `two-bot reconcile-event` CLI (`--list`, then `--created`/`--updated`/`--cancelled`/`--no-effect` with `--execute`). See [the receiver contract](internal-actions-receiver.md); never re-submit the operation under a new key before reconciling. |
| `POST /internal/actions` 404 on staging | The route is dark unless the Worker var `INTERNAL_ACTIONS_INGRESS` (staging env) **and** the secret `TWO_INTERNAL_ACTIONS` are both exactly `1`. Wrong method, a trailing slash or any query string is also 404 by design. Production is always 404. |
| `POST /internal/actions` 503 `unavailable` | The container is not running (public ingress never starts it; wait for the probe or keepalive), the ownership fence refused this deployment, or the receiver answered something other than its JSON envelope. Check `/readyz` and ownership status; do not retry-loop a signed request with a new nonce. |
| `POST /hooks/twitch` refusal or 404 on staging | The EventSub receiver is staging-only and dark unless the Worker route gate and the container listener are both enabled. Read the [EventSub operator runbook](eventsub-runbook.md) for the enable order, secret agreement, challenge echo, and the full refusal matrix before changing anything. |
| Ready but feature inactive | Gateway readiness says nothing about library-only commands/jobs/kill switches. Check [runtime boundaries](#containment-kill-switches-and-feature-flags), not extra environment guesses. |
| Worker restored but Rust regression remains | A bare `wrangler rollback` does not prove image rollback; the `deploy-production` rollback dispatch restores Worker version and container image together with a stable gate. Inspect the active image and use the workflow rollback (or a schema-compatible full redeploy of the known-good pair). |
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
The [October-15 production postmortem](production-incident-2026-10-15-postmortem.md)
records the production gateway crash-loop, its provisional schema-gap cause, and
the rollback receipts; read it before any production gateway recovery.
A denied observation or successful offline test is not a completed staging drill.

### Discord gateway or API outage

**Detection.** Compare `/health` with the `gateway` component of `/readyz`.
Gateway reconnect symptoms and REST send failures are different incidents:
gateway-ready does not prove REST delivery, and HTTP liveness does not prove
either. Use literal `gateway reconnect failed; Twilight will retry`,
`durable gateway initialized; shard connecting` (`resume`) and
`gateway ready; checkpoint committed` (`sequence`). The Worker
`container_readyz_unhealthy` line is a readiness observation, not a Discord
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

**Detection.** Production is live on PlanetScale; Neon hosts the staging
database. Gateway Postgres uses the forwarded `DATABASE_URL` directly.
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
`sticky claim failed; skipping activity` (`reason`, a fixed database class
from `crates/bot/src/db_log.rs` such as `pool_timeout` or `connection`).
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

1. Identify the affected dependency from approved non-secret deployment
   metadata. For staging, [staging configuration](staging-soak.md#provisioning-operator-once)
   records the dedicated `two_bot` DB/role on Neon, separate from the web's
   `two`/`two_app`; production uses PlanetScale. Never derive a replacement URL
   from another application's secrets. The deployed binding must be confirmed,
   not merely assumed from this configuration document.
2. Save both bot health responses and available sanitized startup/checkpoint
   logs. Check the affected provider's status—[Neon](https://neonstatus.com/)
   for staging or PlanetScale for production—and
   [Cloudflare status](https://www.cloudflarestatus.com/) alongside the actual
   dependency owner's evidence. Missing schema/grants, rejected credentials,
   startup failure and provider unavailability require different repairs.
3. Treat inability to persist gateway events/checkpoints as unsafe operation,
   not a license to continue on in-memory state or skip a transaction. Keep
   stored state intact. Gateway-ready does not prove every feature store is
   available; pause manual/operator writes and investigate the specific writer.
4. Check the **deployed** redirect wiring before treating Hyperdrive as an
   active dependency. If `REDIRECT_DB` is present, separate a failed live lookup
   (which serves the configured fallback invite, or 503 when none is valid:
   never the mappings snapshot) from a failed click insert. If absent,
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
| Operational audit kill switch/MAC reasons | Core/store port exists; delivery sweep wired (`crates/bot/src/website_jobs.rs:294`, `crates/bot/src/audit_runtime.rs:141-159`); operator handle for disposable agent-testdb fixtures is the `audit_switch` binary ([usage](audit-store.md#operator-handle)); no halt command for a deployed staging/production store. No fake SQL replacement. |
| Automod, moderation, feature toggles | Forwarded gates, channel-command handlers and gateway automod are implemented and identity/permission gated. This is not blanket builtin coverage or cutover approval; use the current containment table and verify the deployed writer. |
| Health/REST rate limits/429 breaker | Liveness/readiness, durable token admission, executor-local pacing and process-wide guard are implemented. They differ from the legacy control topology; no manual reset endpoint. The private announcement receiver has a separate governor; a legacy writer uses its own runbook. |
| Attribution/scorecard/plan/onboarding/automation reports | Legacy read-only reporting CLIs intentionally dropped by parity decision; core calculations/store presence does not add operator commands. |
| Staging provision/verify/reset and Discord e2e harness | Legacy tooling intentionally dropped; next uses local fixtures/agent-testdb and its own soak evidence. Never reset live/staging databases for tests. |
| Preflight/exact-grant acceptance, Wave 0 exports, Wick whitelist, redesign/role cleanup | No equivalent next operator preflight/export/grant CLI identified. Core permission contracts are not live-grant proof; redesign and third-party dashboard verification are outside routine operations. |
| Cutover import/backfill/capture/dedupe, dual-running, command registration, live identity/bot removal | Cutover tools are separate ports, **not dropped**. Execution is out of scope and has separate gates. This runbook cannot authorize activation, backfill, or legacy-bot removal. Rate limits, cursors/resume, dry-run counts and failure anchors for backfills live in [backfill-runbook.md](backfill-runbook.md). |
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

## Giveaways

`/giveaway create|list|cancel|end|pause|resume|extend|reroll` requires Manage
Server; entry buttons are open to human members with per-interaction role and
age checks. Limits: prize 1–200 characters, duration 1–43200 minutes,
winners 1–100, 10 active giveaways per guild, 1000 entrants each (one paced
membership re-check per entrant must fit the 300 s draw run). Entry
attempts have a bounded process-local 10/minute member budget, and successful
entries share a durable 10/minute guild/member budget across giveaways and
restarts. Duplicates collapse to one row. `gw:<id>` carries only the opaque
id; the server resolves guild, message, deadline, status, requirements and
capacity.

The giveaway row is recorded before its message posts. If sending or the
message-id attachment fails, never blindly re-post: inspect the channel for
a delivered message and cancel the recorded id if needed. An unconfirmed
definition holds an active slot and takes no entries.

`giveaway_countdown` ticks every 15 seconds; durable claims permit at most
one cosmetic refresh at each 1h, 10min, 1min and ending-soon threshold
through the shared REST pacing guard. Count edits are best-effort; the
entry row stays authoritative when a display edit fails.

`giveaway_draw` ticks every 15 seconds: it claims due giveaways under a
10-minute lease, re-validates eligibility from fresh membership reads, runs
the pure draw, commits one draw row and flips to drawn, then announces (edit
the giveaway message, post the winners with seed and revealed nonce, mark
announced). Drawn-but-unannounced rows re-announce from the stored row and
never re-draw; a stale claim with a stored draw skips to announce. Pause
excludes a giveaway from the due scan; resume extends the deadline by the
paused span. Rerolls append a new draw round and leave history intact. No
winner DMs: deferred until a DM send path exists. Staging smoke: create,
enter from two test members, verify two rows plus ephemeral replies, end,
verify the draw row plus winners post, reroll, verify a second round, then
cancel a fresh giveaway and verify a later entry is refused.

### Lifecycle audit and saved-versus-delivered state

Successful pause, resume and extension commands commit one minimal fact in
`automation_audit_log` in the **same transaction** as the lifecycle/deadline
change. `actor_id` is the authenticated staff member, not the giveaway creator;
store-only system calls use NULL. Actions are `giveaway.pause`,
`giveaway.resume` and `giveaway.extend`, with `guild_id`, the giveaway id as
`target_key`, `outcome = 'ok'`, a stable fact `id` and `created_at`; `reason`
stays NULL. No prize/body, entrant roster, seed/nonce, claim capability or raw
database error belongs in these facts or command logs.

Pause still requires an unclaimed open row. Resume locks the paused row and
adds the nonnegative paused span before reopening entries. Extend locks an
open or paused row, adds the validated duration and preserves its status.
Unknown/foreign targets, wrong states and invalid deadlines create neither a
transition nor a success fact. Audit insertion failure rolls back the change
and returns `Giveaway unavailable; try again later.` without a message refresh.
A resume with a backwards clock still changes paused to open and records a fact,
even when its deadline does not move.

A fact's random per-write suffix distinguishes repeated valid extensions and
pause/resume cycles, including equal-timestamp writes. It is **not** a durable
interaction replay key: submitting a fresh extension can extend again. Existing
create/cancel identities and round-numbered draw/reroll audit identities do not
change. Do not infer ordering from the random suffix.

With existing authorized database read access, inspect one guild and giveaway
using this bounded read (replace both placeholders; no new endpoint or grant):

```sql
SELECT id, guild_id, actor_id, action, target_key, outcome, created_at
FROM automation_audit_log
WHERE guild_id = '<guild-id>' AND target_key = '<giveaway-id>'
  AND action IN ('giveaway.pause', 'giveaway.resume', 'giveaway.extend')
ORDER BY created_at DESC, id DESC
LIMIT 50;
```

The result is a capped snapshot, not a full-history export. An empty successful
read means no matching retained facts; a failed read means unknown, not empty.
There is no automatic age-based expiry or actor pseudonymization for this table;
the 45-day `operational_audit_log` cache policy applies to a different table.
Existing [member erasure](privacy.md) deletes entire rows matching guild and
`actor_id`, subject to safety checks, not every fact about that member's
giveaways. NULL/system and other-actor facts can remain. A true guild removal
queues guild-wide deletion, including these facts, but only a successful
executing retention run deletes them: dry-run deletes nothing, rejoin cancels
the queue entry and guards/failures can defer completion. No new retention
window, purge trigger or grant is introduced. Existing MCP/ops audit readers
are not giveaway-audit listing APIs.

The transaction confirms **saved state**, not Discord delivery. The subsequent
message edit is separate and best-effort: `Giveaway paused/resumed/extended; its
message could not be refreshed.` means the state and audit are already saved.
Inspect the saved giveaway and channel before retrying; in particular, do not
repeat an extension just to repair its display. Even a saved success fact does
not prove that the final interaction reply reached Discord. For a repeatable
disposable-DB check, run the protected runtime corpus:

```sh
TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local \
  python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db \
  --test giveaway_actor_audit_acceptance -- --test-threads=1
```
