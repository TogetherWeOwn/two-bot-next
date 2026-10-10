# 48h watch signal query pack (read-only)

Operators watching the bot over a 48-hour window need one read-only answer
per signal from sources that already exist. This pack gives exactly one
query per signal. It installs no monitor, adds no alert rule, and sets no
threshold. Threshold budgets live in the separate thresholds document;
when the two disagree, the thresholds document wins and this pack is the
one to fix.

Rules for every query below:

- Read-only. Scrapes, log filters, and test-container `SELECT` only.
- Never run SQL, probes, or restores against staging or production
  databases. Table queries below run only against an authorized
  test-container copy or a backup artifact.
- Never print secret values, tokens, connection strings, or member data.
- A counter that reset to zero means the process restarted; it does not
  mean the window was quiet.

## Source map

- Metrics exposition: `GET /metrics` on the container-internal listener
  (default `0.0.0.0:8080`), Prometheus text 0.0.4. Off-container, use the
  authenticated Worker route `GET /ops/metrics` with
  `Authorization: Bearer <scrape token>`. Series contract:
  [metrics](metrics.md).
- Container stdout/stderr: Rust `tracing` logs in the Cloudflare dashboard
  for the affected container. Worker/DO tail (`logs`) is not Rust stdout.
  Log names: [runbook](runbook.md#logs-and-keepalive).
- Gateway session contract: [gateway recovery](gateway-recovery.md).
- REST guard and retry contract: [REST guard](rest-guard.md).
- Unban queue contract: `moderation_scheduled_unbans` plus the member
  queue and 25-claims-per-sweep rule in
  [member moderation](member-moderation.md); cutover pending-unban notes in
  [cutover](cutover.md).

Fetch one scrape like this (operator supplies the Worker URL and the
already-provisioned scrape token; the token never goes in a PR or log):

```bash
curl --silent --show-error --max-time 10 \
  -H "Authorization: Bearer ${METRICS_SCRAPE_TOKEN}" \
  "${WORKER_URL}/ops/metrics" > /tmp/watch-start.txt
```

Repeat at the end of the window into a second file and diff the counters.
PromQL snippets below assume a Prometheus-compatible reader over those
scrapes; where no server exists, compare the two files by hand.

## 1. Gateway session starts

Source: `two_bot_gateway_events_total{event="READY"}` (one per fresh
session), `two_bot_gateway_reconnects_total` (HELLOs after the first),
`two_bot_gateway_resumes_total` (accepted RESUMEs). Logs:
`durable gateway initialized; shard connecting` (shows `resume=true|false`),
`gateway shard loop started`.

```promql
sum(increase(two_bot_gateway_events_total{event="READY"}[48h]))
sum(increase(two_bot_gateway_reconnects_total[48h]))
sum(increase(two_bot_gateway_resumes_total[48h]))
```

A rising `READY` count means fresh sessions (IDENTIFY); a rising `RESUMED`
count means continued sessions. Cross-check with one log filter over the
same window: count `gateway shard loop started` lines. The two should move
together; a mismatch means the log window and the scrape window differ.

## 2. REST 429 and 5xx share

Source: `two_bot_rest_requests_total{route,result}` with
`result="429"`, `result="5xx"`, `result="transport"`. The executor records
at response headers, so a later body failure never hides a 429/5xx.

```promql
sum by (result) (increase(two_bot_rest_requests_total[48h]))
sum(increase(two_bot_rest_requests_total{result="429"}[1h]))
  / sum(increase(two_bot_rest_requests_total[1h]))
topk(5, sum by (route) (increase(two_bot_rest_requests_total{result="429"}[48h])))
topk(5, sum by (route) (increase(two_bot_rest_requests_total{result="5xx"}[48h])))
```

Read the hot route from the `route` label before acting; the ported
executor already honors `retry-after` per attempt. Do not hammer Discord,
replay uncertain writes, or invent a breaker reset. The existing 429 alert
rule is unchanged by this pack.

## 3. Action latency

Source: `two_bot_handler_duration_seconds` histogram over nonduplicate
dispatch parse, pipeline, and durable commit, in seconds. Buckets
(`le`): `0.001`, `0.005`, `0.01`, `0.05`, `0.1`, `0.5`, `1`, `5`, `+Inf`,
plus `_sum` and `_count`. This is handler latency, not Discord REST
round-trip time (that is `two_bot_gateway_latency_seconds`, last heartbeat
ACK sample only).

```promql
histogram_quantile(0.50, sum by (le) (rate(two_bot_handler_duration_seconds_bucket[1h])))
histogram_quantile(0.95, sum by (le) (rate(two_bot_handler_duration_seconds_bucket[1h])))
histogram_quantile(0.99, sum by (le) (rate(two_bot_handler_duration_seconds_bucket[1h])))
rate(two_bot_handler_duration_seconds_count[1h])
```

Compare the same quantile across windows (first 24h vs second 24h) rather
than against a fixed budget. A falling `_count` with a rising p99 means
fewer but slower dispatches, not an idle bot.

## 4. Unban-queue depth

There is no live depth gauge; do not invent one here. The queue is the
`moderation_scheduled_unbans` table drained at most 25 claims per sweep
under the owning guild's member queue. The read-only depth query runs
only against an authorized test-container copy or a backup artifact,
never against staging or production:

```sql
SELECT state, COUNT(*)
  FROM moderation_scheduled_unbans
  GROUP BY state;
SELECT COUNT(*)
  FROM moderation_scheduled_unbans
  WHERE execute_at <= now()
    AND state NOT IN ('done', 'cancelled', 'superseded');
```

For the live 48h window, use the closest existing live signals instead
of the table: `periodic job failed` lines in container logs, the
`audit_retry` supervisor streak in
`two_bot_job_consecutive_failures{job="audit_retry"}`, and the moderation
audit mirror deliveries. A growing overdue count in a restored copy plus
job-failure lines in the same window means the sweep is behind; either
alone is only half the story.

## 5. Restart count

Source: container stdout line `listening` (one per process start),
Worker log lines `two-bot container started` / `two-bot container stopped`,
and any `/metrics` counter that dropped to zero between two scrapes
(counters reset on restart).

Log filter over the 48h window (Cloudflare dashboard for the affected
container and Worker tail):

- Count lines matching exactly `listening` in container stdout.
- Count `two-bot container started` in Worker logs.
- Confirm: the same window shows at least one metrics counter reset
  (for example `two_bot_gateway_reconnects_total` back at zero).

Each `listening` line is one process start. A restart without a matching
`container started` line means the process exited inside a running
container (for example a fatal gateway task); a `container started`
without new `READY`/`RESUMED` events means the gateway never connected
after the restart.

## 6. DB-error counter

Source: `two_bot_db_errors_total{op}` (storage-layer failures, not pool
pressure). The `op` label is `admission` (send-admission SQL:
admit/extend/complete) or `other` (failed voice actor store loads, plus
every other store until its operation joins the allowlist).
Fixed cardinality: two series.
Unknown operations collapse to `other`; no error text, query, or identifier
is retained. Series contract: [metrics](metrics.md).

```promql
sum(increase(two_bot_db_errors_total[48h]))
sum by (op) (increase(two_bot_db_errors_total[48h]))
sum(increase(two_bot_db_errors_total{op="admission"}[1h]))
```

Read the `op` label before acting: an `admission`-only burst points at the
send-admission SQL path and its recent deploys, not at the database as a
whole. Do not sum this counter together with
`two_bot_send_admissions_total{outcome="storage_error"}`: the same failure
is counted in both, so adding them double-counts one outage. A counter that
reset to zero between scrapes means the process restarted; it does not mean
the window was quiet.

Alert-threshold sketch (not a threshold): the paging rule fires at 3 or more
storage failures between two keepalive samples, and a restart reset skips
the window rather than firing. A slow trickle below that burst stays silent
here and surfaces instead through `two_bot_job_consecutive_failures`. When
this sketch disagrees with the runbook or the budgets in
[production-deploy](production-deploy.md#signal-thresholds-budgets-and-rollback-triggers),
those win. Runbook: [runbook](runbook.md) (DB errors section).

## 7. Send-admission decisions

Source: `two_bot_send_admissions_total{outcome}` (one `admit()` decision per
increment, not per retry or per completion). The `outcome` label is
`admitted` (Ok), `blocked` (lane or cooldown refusal), `storage_error` (the
admission SQL itself failed; also counted in
`two_bot_db_errors_total{op="admission"}`), or `other` (anything else).
Failed `complete()`/`extend()` storage writes count only in the DB-error
counter above: the admit decision was already recorded. Fixed cardinality:
four series. Series contract: [metrics](metrics.md).

```promql
sum by (outcome) (increase(two_bot_send_admissions_total[48h]))
sum(increase(two_bot_send_admissions_total{outcome="blocked"}[1h]))
sum(increase(two_bot_send_admissions_total{outcome="storage_error"}[48h]))
sum(increase(two_bot_send_admissions_total{outcome="blocked"}[48h]))
  / sum(increase(two_bot_send_admissions_total[48h]))
```

A rising `blocked` share means the token-wide lane in front of every Discord
send is refusing work (held lane or active cooldown), not that Discord
returned 429s; correlate with the container logs for cooldown and held-lane
lines before acting. A rising `storage_error` count is the same outage as
section 6, not a second outage. Do not replay uncertain writes, hammer
Discord, or restart the container to "free" the lane.

Alert-threshold sketch (not a threshold): the paging rule fires only when
windows with *new* `blocked` refusals arrive in 3 consecutive keepalive
samples; one busy tick stays silent, and an idle or self-clearing burst never
pages. Storage failures of the admission SQL page once via the DB-error rule
above, not here. When this sketch disagrees with the runbook or the budgets
in [production-deploy](production-deploy.md#signal-thresholds-budgets-and-rollback-triggers),
those win. Runbook: [runbook](runbook.md) (send admission blocked section).
## 8. Gateway disconnects and missed events

Source: `two_bot_gateway_disconnects_total` (every transport loss the
shard supervisor observed: reconnect failures, close frames, invalid
sessions, cold-resume IDENTIFY) and
`two_bot_gateway_missed_events_total` (dispatches Discord assigned but
this process never received: sequence gaps inside one session). Series
contract: [metrics](metrics.md). This section measures transport and
sequence continuity only. Database health stays with the DB error
counter, and off-container reachability stays with the external uptime
check; neither is repeated here.

```promql
sum(increase(two_bot_gateway_disconnects_total[48h]))
sum(increase(two_bot_gateway_missed_events_total[48h]))
sum(increase(two_bot_gateway_disconnects_total[48h])) > 0
  and sum(increase(two_bot_gateway_resumes_total[48h]))
    + sum(increase(two_bot_gateway_events_total{event="READY"}[48h])) == 0
```

Read the two counters as a pair. The missed-events threshold is zero:
any nonzero increase over the 48h window fails the zero-missed-events
acceptance and is a stop condition under the "Event continuity" row of
the signal-thresholds table in [production-deploy](production-deploy.md)
(zero unexplained gaps). Disconnects are informational on their own —
deploys and host moves cause them — but each one must pair with a later
RESUME or fresh READY in the same window; the third query above fires
when disconnects have no matching session recovery. A rising `READY`
count next to disconnects means fresh IDENTIFYs (checkpoints older than
15 minutes or rejected sessions); a rising `RESUMED` count means the
session continued with no gap. Cross-check with one log filter over the
same window: count `gateway reconnect failed; Twilight will retry` and
`gateway ready; checkpoint committed` lines. A missed-events increase
with no disconnect means the gap predates this instrumentation or the
process restarted mid-window (counters reset to zero on restart, so a
reset is not a quiet window — re-baseline both scrapes after it).

Alert rule: the Worker `gateway_missed_events` rule implements the zero
threshold above — it fires on any increase of
`two_bot_gateway_missed_events_total` between two keepalive samples (the
first sample and counter resets skip the window rather than firing).
Runbook: [runbook](runbook.md#alert-gateway-missed-events).

## 9. Ticker staleness

Source: `two_bot_job_last_success_timestamp_seconds{job}` for the 15 s
tickers `scheduled_messages` and `settings`. A wedged ticker never fails:
skipped busy deadlines count neither as success nor failure, so the
timestamp stops advancing while the failure streak stays flat. Series
contract: [metrics](metrics.md).

```promql
time() - max by (job) (two_bot_job_last_success_timestamp_seconds{job=~"scheduled_messages|settings"} > 0)
```

The `> 0` filter drops never-succeeded series before subtraction, so boot
and parked tickers are absent from the result instead of reading as
billions of seconds stale — matching the Worker `ticker_stale` rule,
which requires a positive timestamp before comparing age.

A zero timestamp means the ticker never succeeded since start (boot) or
was never registered (parked: `DATABASE_URL` unset, or the automations
gate off) — not a wedge. Alert rule: the Worker `ticker_stale` rule pages
when either ticker has no success for more than 10 minutes (40 missed
ticks; boot and parked stay silent). Runbook:
[runbook](runbook.md#alert-ticker-stale).

## What this pack does not do

- No threshold is set or changed here.
- No alert rule, webhook, scrape job, or dashboard is added.
- No database probe is authorized against staging or production.
- No retry, breaker-reset, takeover, rollback, or credential step is
  included; those live in the runbook and cutover docs.
- Dry-run record for the DB-error and send-admission queries (staging
  read-only, offline rehearsal, threshold comparison):
  [watch-signal-dry-run.md](watch-signal-dry-run.md).
