# Cutover dashboard queries and alert thresholds (doc-only)

Status: offline draft. This file installs no monitor, alert rule, scrape job,
dashboard, or webhook. It gives one copy-paste query per cutover panel from
series that already ship, plus warn/page levels the future wiring can reuse.
When this file and a live rule disagree, the live rule wins and this file is
the one to fix.

Scope: DB-error and send-admission signals from the shipped series, plus
gateway and `/readyz` health, plus the EventSub reconcile and feed intake
families (Panel 4). The `db_errors`, `send_admission_blocked` and
`gateway_missed_events` rules already ship (see
[metrics](metrics.md#off-container-scrape-and-alert-rules)). Pool and job
health remain complementary signals, not substitutes for those counters.
Out of scope: installing alerts, paging tests, log-catalog extraction.

Rules for every query: read-only; never run SQL, probes, or restores against
staging or production databases (table queries run only against an authorized
test-container copy or a backup artifact); never print secret values, tokens,
connection strings, or member data. A counter that reset to zero means the
process restarted, not that the window was quiet — re-baseline both scrapes
after a reset.

## Fetch one scrape (all metric panels)

Off-container, the operator supplies the Worker URL and the already-provisioned
scrape token (never in a PR or log). The Container DO pulls this route on every
keepalive tick (default 60 s):

```bash
curl --silent --show-error --max-time 10 \
  -H "Authorization: Bearer ${METRICS_SCRAPE_TOKEN}" \
  "${WORKER_URL}/ops/metrics" > /tmp/cutover-start.txt
```

Repeat at the end of the window into a second file and diff the counters.
PromQL below assumes a reader over those scrapes; with no server, compare the
two files by hand. Series contract: [metrics](metrics.md).

## Panel 1 — Readiness and gateway session health

Owner: cutover executor (on-call operator during the watch).

Live readiness (truthful 200 vs 503 breakdown, not a metrics scrape):

```bash
curl --silent --show-error --max-time 10 --include "${WORKER_URL}/health"
curl --silent --show-error --max-time 10 --include "${WORKER_URL}/readyz"
```

Expect `/health` 200 `{"status":"ok"}` (liveness only) and `/readyz` 200 with
every component ready (the bot serves four):

```json
{"components": [["process", "ready"], ["gateway", "ready"], ["database", "ready"], ["token_invalid", "ready"]]}
```

A 503 names the failing component. For the gateway, `starting` = connecting or
bounded checkpoint I/O and `down` = parked prerequisites. A `database` or
`token_invalid` component at `down` is a fault, never parked: the rollback gates
refuse it, and `token_invalid` returns 503 even with the gateway connected (see
Panel 3). Never use the
invite redirect `/healthz` as gateway health. Source:
[runbook](runbook.md#is-it-alive).

Session continuity from the shipped gateway series (`READY`, accepted RESUMEs,
reconnects, transport disconnects, and missed sequence gaps):

```promql
sum(increase(two_bot_gateway_events_total{event="READY"}[1h]))
sum(increase(two_bot_gateway_resumes_total[1h]))
sum(increase(two_bot_gateway_reconnects_total[1h]))
sum(increase(two_bot_gateway_disconnects_total[1h]))
sum(increase(two_bot_gateway_missed_events_total[1h]))
```

Unpaired-disconnect presence check (hand-diff of the two scrapes; every transport
loss must pair with a later RESUME or fresh READY in the same window):

```promql
sum(increase(two_bot_gateway_disconnects_total[1h])) > 0
  and sum(increase(two_bot_gateway_resumes_total[1h]))
    + sum(increase(two_bot_gateway_events_total{event="READY"}[1h])) == 0
```

A firing result means disconnects with no later RESUME or fresh READY in the
window (same shape as [watch signals §8](watch-signal-queries.md#8-gateway-disconnects-and-missed-events)).
Do not subtract RESUME + READY from disconnects to certify pairing:
`two_bot_gateway_disconnects_total` counts each failed reconnect attempt
through the shard supervisor (`transport_disconnected` in
`crates/bot/src/gateway.rs` runs on every `Err(Reconnect)`, plus close frames,
invalid sessions, and cold-resume IDENTIFY), so one recovered outage with N
failed retries ends in a RESUME yet still leaves a positive subtraction
remainder — while a process start or cold resume contributes a READY or RESUME
with no disconnect, which can hide a real unpaired loss inside the same
window. Match each loss to its recovery by timestamps in the container logs,
not by counts. `two_bot_gateway_missed_events_total` counts
dispatches Discord assigned but this process never received (sequence gaps
inside one session); any increase over the window fails the zero-missed-events
acceptance and the checked-in `gateway_missed_events` rule fires on it. The
live `gateway_unpaired_disconnect` Worker rule now pages when disconnects
have no RESUMED or fresh READY for 3 consecutive keepalive samples (runbook:
[runbook](runbook.md#alert-gateway-unpaired-disconnect)); this hand query
remains as the cross-check, and where they disagree the live rule wins.

Cross-check with one log filter over the same window (container stdout in the
dashboard; Worker tail is not Rust stdout): count `gateway shard loop started`
against `gateway ready; checkpoint committed`. A rising `READY` count
means fresh IDENTIFYs (checkpoint older than 15 minutes or rejected sessions);
a rising `RESUMED` count means the session continued. (`gateway reconnect
failed; Twilight will retry` marks one failed reconnect attempt, not one
transport loss: close frames and invalid sessions increment
`two_bot_gateway_disconnects_total` without logging that line, and one outage
can log it many times before a RESUME.)

| Level | Condition | Action |
|---|---|---|
| Warn | Single 503 sample that recovers; single reconnect paired with a later RESUME or READY | Record the finding with timestamp; keep watching |
| Page | 503 sustained past 60 s after a restart/deploy event; `container_unready_alert` (2 consecutive failed keepalive samples, ~2 min at defaults); reconnects with no later RESUME or fresh READY in the same window | Freeze writers, investigate, evaluate rollback per the 48-hour watch |

Sources: [gateway recovery](gateway-recovery.md),
[container readiness](container-readiness.md),
[production deploy watch](production-deploy.md#48-hour-watch-log-tog-9699).

## Panel 2 — DB-error signals and pool/job health

Owner: on-call operator; DB-side cause belongs to the database dependency
owner. Do not probe staging/production SQL, add grants, or restart the
container to "free" the pool.

Recorded storage failures by producer:

```promql
sum by (op) (increase(two_bot_db_errors_total[1h]))
```

The `db_errors` rule fires at 3+ errors across all `op` labels between two
keepalive samples (resets skip the window). Producers are send-admission SQL
(`op="admission"`) and voice actor load (`op="other"`), not all stores.
Correlate voice load failures with `voice actor load failed; retrying with
capped backoff` in container stdout (a credential refusal logs `voice
actor load failed; credential refused, not retrying`); the warn is throttled to
once per 300 s process-wide, but every failure increments the counter.

Pool saturation (a complementary proxy for DB trouble — SQLx bookkeeping,
not DB reachability):

```promql
two_bot_db_pool_configured == 1
  and two_bot_db_pool_connections == two_bot_db_pool_max_connections
  and two_bot_db_pool_idle_connections == 0
```

The checked-in rule fires after 3 consecutive saturated keepalive samples
(`db_pool_saturated`; see `wrangler/src/alert-rules.ts`
`POOL_SATURATED_SAMPLES`). A short burst that self-clears across the next
samples is not exhaustion.

Job health (DB unreachable surfaces here as failed completions):

```promql
max by (job) (two_bot_job_consecutive_failures) >= 3
time() - two_bot_job_last_success_timestamp_seconds{job="counter"} > 120
```

Substitute 2 x cadence per job for the staleness threshold: `counter` 120,
`rank` 1200, `scheduled_events` 1200, `presence_probe` 7200,
`community_scorecard` 120, `inactivity` 7200. Cadences live in
`JOB_INTERVAL_SECONDS` (`counter` 60, `rank` 600, `scheduled_events` 600,
`presence_probe` 3600, `community_scorecard` 60, `inactivity` 3600). A zero
success timestamp means never succeeded since start (parked/just-started),
not stale — the checked-in rule requires the timestamp above zero and ignores
it. Counter resets skip a window rather than firing. Cross-check container
logs for `periodic job failed` (error class only; payloads are never logged).

Startup failure class (one fixed class per fatal gateway failure, no SQL text):

```bash
curl --silent --show-error --max-time 10 "${WORKER_URL}/readyz" | jq .gateway_failure
```

`store_unavailable`, `gateway_pool_connect_failed`, and
`checkpoint_load_failed` are the DB-side classes; each is also logged once as
`durable gateway failed; checkpoint unchanged, readiness unavailable` and kept
for 15 s on `/readyz` (the startup linger; a running-gateway failure lingers
75 s) plus Worker-side `container_gateway_failure`. Full
vocabulary: [startup diagnostics](startup-diagnostics.md), [runbook](runbook.md#logs-and-keepalive).

| Level | Condition | Action |
|---|---|---|
| Warn | One saturated pool sample; 1–2 consecutive job failures; single failure-class observation under investigation | Check the dependency status and recent deploys for a new query path or widened fan-out; keep watching |
| Page | `db_pool_saturated` (max pool, zero idle, 3 consecutive samples); `job_consecutive_failures:<job>` at 3+; `job_stale:<job>` (no success for 2+ cadences); persisting store/checkpoint failure class | Freeze writers, fix the holder/dependency; roll back only if a new query path in this revision holds checkouts |

Sources: [metrics](metrics.md#off-container-scrape-and-alert-rules),
[runbook Alert: DB pool](runbook.md#alert-db-pool).

## Panel 3 — Send-admission and REST guard signals

Owner: on-call operator; containment uses the actual writer's verified control
([runbook containment](runbook.md#containment-kill-switches-and-feature-flags)).
Never hammer Discord, replay uncertain writes, or invent a breaker reset.

Send-admission decisions by outcome:

```promql
sum by (outcome) (increase(two_bot_send_admissions_total[1h]))
```

The `send_admission_blocked` rule fires on new `outcome="blocked"` refusals
in 3 consecutive keepalive windows; idle windows (no new blocked refusals)
and counter resets clear the streak. The first sample after monitoring arms
counts as a window whenever the blocked counter is already nonzero.
Admission SQL failures instead increment
`two_bot_db_errors_total{op="admission"}` and feed `db_errors`.

REST 429 share (`rest_429_rate`) plus the hot route:

```promql
sum(increase(two_bot_rest_requests_total{result="429"}[1h]))
  / sum(increase(two_bot_rest_requests_total[1h]))
topk(5, sum by (route) (increase(two_bot_rest_requests_total{result="429"}[1h])))
topk(5, sum by (route) (increase(two_bot_rest_requests_total{result="5xx"}[1h])))
topk(5, sum by (route) (increase(two_bot_rest_requests_total{result="transport"}[1h])))
```

The rule fires above 10% 429s between two keepalive samples with at least 10
requests in the window (`rest_429_rate`). The executor honors `retry-after`
per attempt, so read the `route` label first and confirm no deploy is in
progress before acting. The 5xx share has the same 10% / 10-request rule
(`rest_5xx_rate`); only `transport` shares stay informational with no rule.
Counter resets skip the window.

Guard and token state (private `/metrics` `two_bot_rest_guard_*` + logs + `/readyz`):

- `discord_breaker_open` — rolling 401/403/429 count reached 5000 per 600 s;
  non-essential REST refuses pre-wire while open. Stop the offending workload,
  fix the permission/caller bug, let the window cool.
- `discord_global_pause` — Discord global 429 deadline in force; let it elapse,
  do not resubmit concurrent copies.
- `discord_token_invalid` — a bot-authenticated 401 latched `token_invalid`;
  `/readyz` adds the component at `down` (503). Stop retries; provisioning is
  a governed path, not a rollback. Contract: [REST guard](rest-guard.md).

Private scrape series: `two_bot_rest_guard_breaker_open`,
`two_bot_rest_guard_token_invalid`, `two_bot_rest_guard_global_paused`, plus
`two_bot_rest_guard_rejected_requests_total` and
`two_bot_rest_guard_invalid_requests_total` (full list:
[REST admission guard](metrics.md#rest-admission-guard)). A zero
`two_bot_rest_guard_global_pause_remaining_seconds` does not mean admission is
open while `two_bot_rest_guard_pending_global_responses` is nonzero.
Three metrics rules page on this family: [REST token
invalid](runbook.md#alert-rest-token-invalid) on the latched token, [REST
refusal surge](runbook.md#alert-rest-refusal-surge) on invalid/refused bursts,
and [REST global responses
stuck](runbook.md#alert-rest-global-responses-stuck) on unresolved pause
bodies; the paused/remaining gauges stay rule-free by design (see the stuck
rule's runbook).

Admission lane state (durable gate in `public.discord_send_admission`: one row
per credential fingerprint, 60 s self-heal lease on `in_flight`, no
drop-release). Read-only, test copy or backup artifact only — never
staging/production:

```sql
SELECT generation, in_flight, in_flight_since_ms, indefinite, hold_until_ms
  FROM public.discord_send_admission;
```

`in_flight = true` with a fresh `in_flight_since_ms` and no completing
exchange is a live send: leave it alone. A stamp older than the 60 s lease is
reclaimed automatically by the next `admit` (watch for the `reclaimed a stale
in-flight lane` warning). Only an `indefinite` hold needs explicitly
authorized reconciliation (fence all credential users, prove the old send
cannot continue, record evidence for the exact generation) — never a startup
reset or force-send. A `hold_until_ms` far in the future refuses every new
send until the durable hold allows it. Contract:
[send admission](discord-send-admission.md).

| Level | Condition | Action |
|---|---|---|
| Warn | Single-window 429 share above 10% (min 10 requests); any 5xx/transport spike with a named hot route; single global pause | Read the hot route, confirm no deploy is in progress, contain through the writer's verified control |
| Page | `rest_429_rate` firing across consecutive windows after containment; breaker open; `token_invalid` latched; indefinite hold or abandoned `in_flight` blocking sends | Stop the workload / freeze writers; reconcile the lane through the authorized path; roll back only if the new revision caused it |

Sources: [metrics](metrics.md#off-container-scrape-and-alert-rules),
[runbook Alert: REST 429](runbook.md#alert-rest-429),
[REST guard](rest-guard.md),
[send admission](discord-send-admission.md).

## Panel 4 — EventSub reconcile-error growth and feed error share

Owner: on-call operator; reconcile cause belongs to the EventSub
subscription owner, feed cause to the feed source or the platform per the
breakdown below. Read-only: no Helix request, no subscription change, no
feed edit, no replay.

### 4a. Reconcile-error growth (burn between scrapes)

Failure sites by the same fixed `error_class` strings as the reconcile
log field, plus the successful-mutation counters for the same window:

```promql
sum(increase(two_bot_eventsub_reconcile_errors_total[1h]))
sum by (error_class) (increase(two_bot_eventsub_reconcile_errors_total[1h]))
sum by (action) (increase(two_bot_eventsub_reconcile_actions_total[1h]))
```

`error_class` is `reconcile_action_failed`, `reconcile_prior_failed`,
`reconcile_unhealthy`, `reconcile_failed`, or `other`;
`action` is `create` or `delete` (2xx only; 409 already-exists
confirmations fold into the pass outcome). Passes run at startup and on
the bounded hourly interval, so these deltas move at most once per pass
plus the startup pass — a flat window with a healthy Q2 outcome is
steady state, not a gap. Counter resets skip the window rather than
firing.

Errors are detail, not the ticket: the checked-in
`eventsub_reconcile_failed` rule tickets when
`two_bot_eventsub_reconcile_passes_total{outcome="failed"}` rises
between samples (first sample is baseline-only; a healthy-pass increase
with no failed increase resolves). Join a nonzero error delta to the Q2
log lines by `error_class` before acting:

```promql
sum(increase(two_bot_eventsub_reconcile_passes_total{outcome="failed"}[1h]))
```

| Level | Condition | Action |
|---|---|---|
| Warn | Error growth with no failed-pass growth (healed or verification-pending) | Record the `error_class` breakdown with timestamp; run the Q2 read-only reconcile queries and keep watching |
| Page | Any failed-pass growth (the ticket fires on a single positive delta) | Follow [runbook Alert: eventsub reconcile failed](runbook.md#alert-eventsub-reconcile-failed): read-only diagnosis only — no Helix request, no subscription change, no restart to "clear" the counter |

Sources: [metrics](metrics.md#off-container-scrape-and-alert-rules),
[runbook Alert: eventsub reconcile failed](runbook.md#alert-eventsub-reconcile-failed),
[subscription-health queries](eventsub-subscription-health-queries.md#q2--last-reconcile-outcome).

### 4b. Feed error share (rate over the window)

Failed fetches over all fetch attempts, plus the per-outcome and
per-class breakdowns:

```promql
sum(increase(two_bot_feed_fetch_errors_total[1h]))
  / sum(increase(two_bot_feed_fetches_total[1h]))
sum by (outcome) (increase(two_bot_feed_fetches_total[1h]))
topk(5, sum by (error_class) (increase(two_bot_feed_fetch_errors_total[1h])))
```

`outcome` is `success`, `invalid_source`, `refused_redirect`, or
`failure`; `error_class` is `invalid_source`, `non_public_address`,
`redirect_refused`, `too_large`, `invalid_encoding`,
`unsupported_content_type`, `unsupported_encoding`, `resolve`,
`transport`, `deadline`, `http_error`, or `other` (successes record
nothing in the errors family). Labels are per-outcome and per-class,
never per-feed: no source URL, host, or feed id reaches a label, so a
single bad relay is visible only as its class. Counter resets skip the
window rather than firing.

| Level | Condition | Action |
|---|---|---|
| Warn | Nonzero share concentrated in one class across 2+ consecutive passes, or any `invalid_source` growth | Source-side: check the source's availability and recent fetch status through authorized diagnostics; do not copy response bodies. See [feed supervisor](feed-supervisor.md#failure-signals-and-triage) |
| Page | Share above 50% over 1h across classes, or errors paired with `timeout` / `database` / `rest` job classes | Platform-side: treat as platform, not feed config; see the operations runbook. For `feed` / `recovery_required` job classes follow [runbook Alert: job failures](runbook.md#alert-job-failures) — never repost to "fix" a bounded history miss |

Sources: [metrics](metrics.md#off-container-scrape-and-alert-rules),
[feed supervisor](feed-supervisor.md#failure-signals-and-triage),
[runbook Alert: job failures](runbook.md#alert-job-failures).

## Threshold summary

| Panel | Warn (record, keep watching) | Page (freeze writers, decide rollback) |
|---|---|---|
| Readiness / gateway | Single 503; paired reconnect | 503 past 60 s; unready alert; reconnects with no RESUME/READY; any missed-events increase |
| DB errors / pool / jobs | 1 saturated sample; 1–2 job failures | `db_errors` (3+ between samples); pool saturated x3; 3 consecutive failures; job stale; store/checkpoint class persists |
| Send admission / REST | Single 429 window; 5xx/transport spike; one global pause | `send_admission_blocked` (3 windows); repeat 429 after containment; breaker open; token invalid; indefinite/wedged lane |
| Reconcile errors / feed share | Error growth with no failed-pass growth; single-class feed errors across passes | Any failed-pass growth (ticket firing); feed share >50% over 1h or paired with platform job classes |

## What this doc does not do

- No threshold is set or changed here; the checked-in rules in
  `wrangler/src/alert-rules.ts` and the watch budgets in
  [production deploy](production-deploy.md) are unchanged.
- No alert rule, webhook, scrape job, or dashboard is added.
- No database probe is authorized against staging or production.
- No retry, breaker-reset, takeover, rollback, or credential step is included;
  those live in the runbook and cutover docs.
