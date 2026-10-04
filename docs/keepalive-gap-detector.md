# Keepalive-gap detector spec (offline, doc-only)

Status: offline proposal. This file installs no monitor, sets no live
threshold, and changes no alert, dashboard, scrape job, webhook, or secret.
The query examples are dashboard input, not deployed rules or permission to
query a live system. Existing cutover, read-access and paging gates still
apply; this proposal does not override their budgets or thresholds.

Scope: the single always-on container's Discord gateway heartbeat path
(opcode 10 HELLO, opcode 1 heartbeat, opcode 11 ACK), using the already-shipped
metrics. The Container Durable Object (DO) keepalive tick is a separate
observer. No new telemetry, paging test or reconnect tuning belongs here.

## 1. Separate the three observations

| Observation | Existing evidence | What it cannot prove |
| --- | --- | --- |
| G1: gateway ACK inactivity | Adjacent `two_bot_gateway_events_total{event="HEARTBEAT_ACK"}` counter values, with recovery and eligibility checks below | A flat counter alone does not prove a connected zombie; startup, recovery, a parked gateway or a restart can also be quiet |
| Scrape availability | Timestamp of a stored `/ops/metrics` scrape; missing series or failed scrape recorded separately | Scrape time is not the last ACK time and is not a keepalive-tick receipt |
| G2: DO keepalive-tick liveness | A positive, time-bounded tick observation or the existing `container_keepalive_arm_failed` failure signal | Healthy ticks need not emit alert/recovery lines; silence in those logs does not prove a dead tick |

`/ops/metrics` fetches the Rust process directly, independently of the
DO's saved readiness observation. The DO persists `lastProbeAt` privately;
the current public metrics/readiness contract exposes no last-tick timestamp.
Consequently these queries **cannot establish G2 liveness or distinguish
G1 from G2**. External uptime measures HTTP availability, not tick execution.
Record tick liveness as **unknown** without independent positive evidence;
use the existing monitoring-outage path for an arm failure. Do not add a
metric or live monitor as a workaround in this doc-only change.

The gateway interval comes from Discord's HELLO, not `KEEPALIVE_SECONDS`.
The default DO cadence is 60 seconds; it is not the gateway heartbeat
cadence and does not timestamp these independent scrapes. An illustrative
41.25-second HELLO interval would mean approximately 3 missed ACK opportunities
in 120 seconds and 7 in 300 seconds. Those are estimates, not measured beat
counts or a hard-coded Discord interval.

## 2. Proposed thresholds and mandatory eligibility

Thresholds count **quiet observation intervals**, not samples or DO ticks.
At a fixed 60-second scrape cadence, two intervals require three observations
spanning 120 seconds; five require six observations spanning 300 seconds.
Each interval compares its two adjacent observations in the same process
and session epoch. The thresholds are not live alert rules.

| Level | Condition | Action |
| --- | --- | --- |
| Watch | Two consecutive eligible quiet intervals spanning at least 120 seconds | Watcher records one finding with disposition record-and-watch |
| Page candidate | Five consecutive eligible quiet intervals spanning at least 300 seconds | Watcher hands the finding to the on-call operator for investigation and the existing paging path, if applicable; this proposal sends no page |

Before either level is classified, require **all** of the following:

1. Successful, timestamped observations on the declared scrape schedule,
   with all four counters present: ACK, reconnect, RESUME and READY. Missing,
   stale, invalid or skipped observations are **unknown**, never zero activity.
   A missed scheduled observation breaks the streak; do not bridge it with
   a later scrape. Record the actual elapsed span, not just a sample count.
2. Positive session evidence for the watched process/revision (for example,
   its READY/RESUMED event and current `/readyz` gateway `ready`), and gateway
   `ready` at each observation. Gateway `down` with prerequisites missing is
   parked/ineligible; `starting`, failed dials or active recovery belong to
   [session investigation](watch-signal-queries.md#8-gateway-disconnects-and-missed-events).
3. No process/revision boundary and no observed cumulative-counter decrease
   between adjacent observations. **Any decrease**, including 100 to 2, is
   reset evidence: discard the streak and re-baseline all counters. Absence
   of a decrease does not prove continuity: a restarted counter can catch
   up before the next scrape. Cross-check revision and positive process/startup
   evidence from the existing operator record. No process-start/uptime metric
   is emitted; revision alone cannot distinguish a same-revision restart.
   If continuity cannot be established, classify as unknown.
4. No increase in reconnect, RESUME or READY across any interval. These three
   gates apply to **both** watch and page-candidate levels. A rise resets the
   streak and routes the finding to the existing session-recovery path.
   Reconnects count later successful HELLOs, not failed dial attempts; flat
   recovery counters alone are not evidence of a connected session.

With eligibility satisfied, ACK delta zero extends the streak; ACK delta
positive clears it. NaN latency and zero ACKs do **not** establish
pre-HELLO: HELLO/READY does not initialize latency; an ACK without an RTT
measurement can leave NaN, and later HELLOs clear the previous RTT.
A positively established ready session that never receives its first ACK
is eligible and can reach these thresholds. Without positive session
evidence, record startup/session **unknown**, clear the streak and hand the
finding to the operator at the next watch checkpoint; never label it healthy
or silently exclude it forever.

For a non-60-second observation schedule, record the cadence and require
complete adjacent coverage over the same 120/300-second spans. Do not reuse
the query templates' three/six-sample guards as if they were cadence-neutral.

## 3. Dashboard query templates (read-only, not a complete detector)

These examples assume a Prometheus-compatible reader scoped to **exactly one
watched target/process**, with the four series sampled together every 60
seconds. Select the correct target in that reader before use; summing across
revisions, staging targets or multiple processes can hide gaps and resets.
No deployed reader, scrape job or target name is created by this spec.
For a manual watch, use the same metric contract and adjacent observations
in §4; fetching only two window-edge files cannot prove consecutive coverage.

**Required alignment for the two inactivity screens:** evaluate the five-minute
and two-minute screens below as instant queries at a completed scrape's
**stored timestamp**, after all four current samples are present at that same
timestamp. Do not use the dashboard's wall-clock "now" for these screens or
assume a 60-second evaluation step shares the scrape phase. A reader that cannot
select or verify this timestamp has **unknown/unsupported screen coverage**;
use the complete manual adjacent-observation path in §4 instead. The separate
scrape-age query below must use current evaluation time, not this stored-time
alignment. Do not change a scrape job or dashboard to satisfy this offline
proposal.

Prometheus ranges are left-open/right-closed. At that aligned evaluation time,
the one-second padding below includes the baseline at the exact 120/300-second
boundary. For scrapes at `t=0,60,120,...`, `[5m1s]` at `t=300` selects
`(-1,300]` and includes six observations. At `t=330` it selects `(29,330]`
and includes only five; `[2m1s]` at `t=150` similarly includes only two.
Even exactly one second after the scrape, the baseline is the excluded left
endpoint. Complete 60-second scrapes can therefore produce empty screens
indefinitely at an off-phase evaluation; that is not evidence of ACK activity.

Minimum sample counts and explicit reset predicates reject incomplete aligned
ranges and observed counter resets, which `increase()` otherwise adjusts away.
They **do not** establish readiness, process continuity or evenly spaced
coverage: inspect the stored timestamps and apply every §2 gate. With unknown
alignment, jitter or a different cadence, a template may omit a boundary or
retain earlier activity; adjudicate the complete adjacent-observation record,
not an empty query result. A result is a screening hit, **not** an automatic
page; no result is not a claim of health.

Five-minute scrape-aligned screening shape (six observations, five quiet
intervals):

```promql
sum(increase(two_bot_gateway_events_total{event="HEARTBEAT_ACK"}[5m1s])) == 0
  and sum(increase(two_bot_gateway_reconnects_total[5m1s])) == 0
  and sum(increase(two_bot_gateway_resumes_total[5m1s])) == 0
  and sum(increase(two_bot_gateway_events_total{event="READY"}[5m1s])) == 0
  and sum(resets(two_bot_gateway_events_total{event="HEARTBEAT_ACK"}[5m1s])) == 0
  and sum(resets(two_bot_gateway_reconnects_total[5m1s])) == 0
  and sum(resets(two_bot_gateway_resumes_total[5m1s])) == 0
  and sum(resets(two_bot_gateway_events_total{event="READY"}[5m1s])) == 0
  and min(count_over_time(two_bot_gateway_events_total{event="HEARTBEAT_ACK"}[5m1s])) >= 6
  and min(count_over_time(two_bot_gateway_reconnects_total[5m1s])) >= 6
  and min(count_over_time(two_bot_gateway_resumes_total[5m1s])) >= 6
  and min(count_over_time(two_bot_gateway_events_total{event="READY"}[5m1s])) >= 6
```

Two-minute scrape-aligned screening shape (three observations, two quiet
intervals), with the same stored-timestamp requirement and
recovery/reset/coverage gates:

```promql
sum(increase(two_bot_gateway_events_total{event="HEARTBEAT_ACK"}[2m1s])) == 0
  and sum(increase(two_bot_gateway_reconnects_total[2m1s])) == 0
  and sum(increase(two_bot_gateway_resumes_total[2m1s])) == 0
  and sum(increase(two_bot_gateway_events_total{event="READY"}[2m1s])) == 0
  and sum(resets(two_bot_gateway_events_total{event="HEARTBEAT_ACK"}[2m1s])) == 0
  and sum(resets(two_bot_gateway_reconnects_total[2m1s])) == 0
  and sum(resets(two_bot_gateway_resumes_total[2m1s])) == 0
  and sum(resets(two_bot_gateway_events_total{event="READY"}[2m1s])) == 0
  and min(count_over_time(two_bot_gateway_events_total{event="HEARTBEAT_ACK"}[2m1s])) >= 3
  and min(count_over_time(two_bot_gateway_reconnects_total[2m1s])) >= 3
  and min(count_over_time(two_bot_gateway_resumes_total[2m1s])) >= 3
  and min(count_over_time(two_bot_gateway_events_total{event="READY"}[2m1s])) >= 3
```

Stored ACK-series **scrape age** in seconds, only while an instant sample is
available under the reader's lookback/staleness rules:

```promql
time() - timestamp(two_bot_gateway_events_total{event="HEARTBEAT_ACK"})
```

**Evaluate scrape age at current time:** use an instant query with the reader's
current evaluation timestamp ("now"), independently of the two inactivity
screens' historical scrape-aligned evaluation. Prometheus `time()` returns
**evaluation time**, not the physical wall clock. If the latest stored sample
is at `t=0` and the current evaluation is at `t=240`, the age is 240 seconds
while that sample remains selectable. Evaluating the same expression at `t=0`
returns zero; that historical result cannot measure current scrape age.

A freshly scraped but unchanged ACK counter has scrape age near zero, even
if the last ACK was ten minutes ago. An absent/stale instant series returns
no value, not a large age. Show that as unknown/missing, never coalesce it to
zero or healthy. This query supplies neither last-ACK age nor last-tick age,
and does not establish the inactivity screens' coverage or eligibility.

Correlate container stdout for the watched process (Worker tail is not Rust
stdout): `gateway ready; checkpoint committed`, session recovery, and
`gateway reconnect failed; Twilight will retry`. These are corroborating
observations, not a proof of tick execution or continuous connection.
See the [event catalog](observability-event-catalog.md) for spellings.

## 4. `T_0` owner and advancing observation baseline

Owner: the cutover executor. At `T_0` (first `/readyz` 200 on the production
revision), record an initial observation alongside the
[watch header](watch-run-record.md#1-watch-header-fill-once-at-t_0):

- UTC timestamp, revision and available process/startup evidence;
- ACK, reconnect, RESUME and READY counter values;
- latency value (NaN means no current RTT measurement, not no HELLO/ACK);
- gateway readiness and positive session evidence, or explicitly unknown;
- observation cadence and DO keepalive cadence **separately**;
- if an inactivity screen is used, its aligned evaluation timestamp and the
  matching four stored sample timestamps, or screen coverage unknown with the
  manual path used; record scrape age separately at current evaluation time;
- metric/log evidence pointers; tick liveness unknown unless positively shown.

Preserve T0 for audit context only. Each later observation compares against
**the immediately previous eligible observation**, then becomes the next
baseline. Retain enough adjacent timestamps and counter values to cover the
full watch/page-candidate window (three/six at 60 seconds). Restart/recovery/unknown breaks the
streak and requires a new baseline; do not compare across process boundaries.

Example: T0 ACKs=10, later ACKs=100, then six observations at 100 over five
minutes. Deltas against T0 stay +90 and hide the stall; adjacent deltas are
`[0, 0, 0, 0, 0]` and reach the page-candidate threshold if all gates hold.
A missing baseline or observation is a watch-coverage gap, not a waiver.

## 5. Ack step

- Watch-level finding: watcher on shift records one
  [signal row](watch-run-record.md#3-signal-check-rows), the actual covered
  interval, all eligibility checks and disposition record-and-watch. No page.
- Page-candidate finding: watcher hands the evidence to the on-call operator;
  if the existing incident/paging process emits a page, the operator owns its
  15-minute ack bound. Unacked past 30 minutes escalates to the watch lead
  under the [existing ladder](watch-ack-owners.md#3-escalation-ladder).
- Unknown coverage/session/tick evidence is recorded explicitly and handed
  to the operator/watch lead through existing paths, not cleared as healthy.
- An incident open at a checkpoint forces EXTEND or ROLLBACK, never GO
  ([checkpoint annotations](watch-run-record.md#5-checkpoint-gono-go-annotations)).

No rule id, paging path or escalation level is created here. A future wiring
change must name the rule, supply its missing observation/eligibility evidence,
link its runbook section and update this spec in the same reviewed PR.

## 6. First response (investigation only)

1. Confirm complete adjacent observations, reset/recovery predicates and
   positive gateway/session evidence before classifying ACK inactivity.
2. Separate successful scrapes from tick evidence. If the tick is unobservable,
   say unknown; do not infer G2 from scrape age or transition-log silence.
3. Correlate `/readyz`, revision/startup evidence and container session logs.
   A parked/starting gateway follows the existing readiness/recovery runbook.
4. Record the finding, coverage and ack ownership. Do not restart the bot,
   reset a breaker, replay writes, or change permissions, intents or secrets.
   Any operational response retains its existing runbook/approval gates.

## Offline validation and sources

The review evidence carries reproducible synthetic `promtool test rules`
fixtures for all three literal query blocks, plus offline cases for the manual
eligibility/baseline gates. Alignment cases cover both thresholds at offsets
0, 1, 2, 30 and 59 seconds across consecutive cycles: aligned complete quiet
windows screen positive; tested off-phase windows return empty and require
manual adjudication, not a healthy classification. Separate scrape-age fixtures
contrast the same stored `t=0` sample evaluated at `t=0` (age zero) and current
`t=240` (age 240), and require no result for missing, stale or expired instant
samples. They use no live scrape, Prometheus server, database, staging or
production system. Syntax/fixture evaluation is not evidence that a live reader,
target, cadence, eligibility integration or tick signal exists.
See the PR verification section for the exact validator version and results.

- Metric semantics: `crates/bot/src/gateway_metrics.rs`,
  `crates/core/src/metrics.rs`, [metrics contract](metrics.md).
- Worker scrape and private readiness state: `wrangler/src/index.ts`,
  [container readiness](container-readiness.md).
- [Session signals](watch-signal-queries.md#8-gateway-disconnects-and-missed-events),
  [event catalog](observability-event-catalog.md),
  [watch run record](watch-run-record.md), [ack owners](watch-ack-owners.md),
  [watch handover](cutover-watch-handover.md).
- Prometheus [range endpoints](https://prometheus.io/docs/prometheus/latest/querying/basics/#range-vector-selectors)
  and [staleness](https://prometheus.io/docs/prometheus/latest/querying/basics/#staleness).
- Prometheus [increase](https://prometheus.io/docs/prometheus/latest/querying/functions/#increase),
  [resets](https://prometheus.io/docs/prometheus/latest/querying/functions/#resets),
  [sample counts](https://prometheus.io/docs/prometheus/latest/querying/functions/#aggregation_over_time)
  [timestamp](https://prometheus.io/docs/prometheus/latest/querying/functions/#timestamp)
  and [evaluation time](https://prometheus.io/docs/prometheus/latest/querying/functions/#time).
- Prometheus [offline expression tests](https://prometheus.io/docs/prometheus/latest/configuration/unit_testing_rules/).
