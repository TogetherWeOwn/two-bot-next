# Keepalive-gap detector spec (offline, doc-only)

Status: offline draft. This file installs no monitor, adds no alert rule,
sets no threshold, changes no scrape job, dashboard, webhook, or secret,
and touches no staging or production system. It specifies the
missed-heartbeat threshold sketch, one copy-paste dashboard query pack for
gateway keepalive gaps, the `T_0` owner, and the ack step. When this file
and a live rule disagree, the live rule wins and this file is the one to
fix.

Scope: the single always-on container's Discord gateway heartbeat path
(opcode 10 HELLO interval, opcode 1 heartbeats, opcode 11 ACKs) as observed
through the already-shipped series, plus the Container keepalive tick that
samples them. Out of scope: wiring a new alert rule, paging tests,
scrape-job changes, log-catalog extraction, reconnect tuning.

## 1. The two gaps (do not conflate them)

| # | Gap | What stalls | Observed through | Covered today by |
| --- | --- | --- | --- | --- |
| G1 | Gateway heartbeat-ACK gap | Discord opcode-11 ACKs stop arriving while the shard loop claims to be connected (zombie connection, stalled reception task) | `two_bot_gateway_events_total{event="HEARTBEAT_ACK"}` stops increasing; `two_bot_gateway_latency_seconds` goes stale | Nothing pages on this; disconnect/missed-event counters cover transport loss, not a silent-but-connected shard |
| G2 | Keepalive-sample gap | The Container DO tick itself stops sampling (Worker down, alarm lost, `container_keepalive_arm_failed`) | No new keepalive samples at all: no alert/recovery lines, no metric deltas | `container_keepalive_arm_failed` log + the external uptime check ([watch-external-uptime](watch-external-uptime.md)) |

This spec's threshold sketch is for **G1**. G2 reuses the existing
monitoring-outage path; §4 gives only the dashboard staleness query that
tells the two apart.

Background cadence: Discord's HELLO carries the heartbeat interval
(typically ~41.25 s), so a healthy shard completes roughly 1.4 ACKs per
60 s keepalive sample. Expecting at least one new ACK per sample is the
normal case, not a tight bound: interval jitter, an in-flight heartbeat
at the sample edge, and a clean reconnect all legitimately produce a
single quiet sample.

## 2. Missed-heartbeat threshold sketch (not a threshold)

Read the ACK counter together with the session-recovery series from the
[query pack](watch-signal-queries.md#8-gateway-disconnects-and-missed-events):
`two_bot_gateway_reconnects_total` (new HELLOs),
`two_bot_gateway_resumes_total` (accepted RESUMEs), and
`two_bot_gateway_events_total{event="READY"}` (fresh IDENTIFYs). A quiet
ACK counter next to rising reconnects is transport loss in progress
(§8 owns it); a quiet ACK counter next to flat reconnects is the zombie
this spec describes.

| Level | Condition (consecutive keepalive samples, default 60 s cadence) | Action |
| --- | --- | --- |
| Watch | 0 new `HEARTBEAT_ACK`s across 2 consecutive samples (~2 min, ~3 missed beats), session-recovery series flat | Watcher on shift records a §5 finding (record-and-watch); keep watching |
| Page candidate | 0 new `HEARTBEAT_ACK`s across 5 consecutive samples (~5 min, ~7 missed beats), session-recovery series flat, gateway component not `down`/parked | On-call operator investigates per §6; pages like any other sustained outage signal |

Exclusions (a sample meeting these never extends the streak):

- Pre-first-HELLO: `two_bot_gateway_latency_seconds` is `NaN` and the ACK
  counter has never left zero since process start (no heartbeat negotiated
  yet, not a gap).
- Parked prerequisites: `/readyz` gateway is `down` with the
  `gateway prerequisites missing` line (no shard running, not a zombie).
- Process restart: any `/metrics` counter dropped to zero between the two
  scrapes (counters reset on restart) — re-baseline both scrapes, restart
  the streak.
- Clean reconnect in progress: reconnects, RESUMEs, or READYs rose in the
  same window (transport owned by §8, not this spec).

## 3. Dashboard queries (read-only)

Same scrape method as the query pack: the operator supplies the Worker
URL and the already-provisioned scrape token (never in a PR or log),
fetches `${WORKER_URL}/ops/metrics` into two files at the window edges,
and diffs the counters. PromQL below assumes a Prometheus-compatible
reader over those scrapes; with no server, compare the two files by hand.
Never run these against staging or production databases; they read metric
scrapes only. Series contract: [metrics](metrics.md).

G1 gap detector — zero new heartbeat ACKs over 5 minutes with no
concurrent session recovery (the page-candidate shape):

```promql
sum(increase(two_bot_gateway_events_total{event="HEARTBEAT_ACK"}[5m])) == 0
  and sum(increase(two_bot_gateway_reconnects_total[5m])) == 0
  and sum(increase(two_bot_gateway_resumes_total[5m])) == 0
  and sum(increase(two_bot_gateway_events_total{event="READY"}[5m])) == 0
```

G1 early-watch shape — same test over 2 minutes:

```promql
sum(increase(two_bot_gateway_events_total{event="HEARTBEAT_ACK"}[2m])) == 0
  and sum(increase(two_bot_gateway_reconnects_total[2m])) == 0
  and sum(increase(two_bot_gateway_resumes_total[2m])) == 0
```

G1-vs-G2 disambiguator — ACK staleness in seconds (large value with fresh
keepalive samples means G1; large value with no fresh samples of any
series means G2, the tick itself is gone):

```promql
time() - timestamp(two_bot_gateway_events_total{event="HEARTBEAT_ACK"})
```

Cross-check with one log filter over the same window (container stdout in
the Cloudflare dashboard for the affected container; Worker tail is not
Rust stdout): count `gateway shard loop started` against
`gateway ready; checkpoint committed` and look for
`gateway reconnect failed; Twilight will retry`. No reconnect-failed
lines plus a flat ACK counter is the zombie shape; reconnect-failed lines
plus a flat ACK counter is transport loss (§8). Event spellings:
[observability event catalog](observability-event-catalog.md).

Query validation receipt: every query above was checked offline on
2026-10-04 with a local validator (balanced delimiters, function names in
the PromQL allowlist, metric/label names and label values against the
[metrics](metrics.md) contract and the `crates/core/src/metrics.rs`
allowlists, valid range durations). The queries were never executed
against staging or production. See the PR verification section for the
validator output.

## 4. `T_0` owner and baseline step

Owner: the cutover executor. At `T_0` (first `/readyz` 200 on the
production revision), alongside copying §1 of the
[run-record sheet](watch-run-record.md#1-watch-header-fill-once-at-t_0)
onto the execution card, the executor records one baseline line on the
card:

- `HEARTBEAT_ACK` counter value + scrape UTC time,
- `two_bot_gateway_latency_seconds` value (`NaN` is expected pre-first-ACK),
- keepalive cadence in effect (`KEEPALIVE_SECONDS` or the default 60 s).

Every later gap reading diffs against this baseline, not against zero.
A missing baseline line is a gap in the watch, not a silent waiver.

## 5. Ack step

- Watch-level gap (2 quiet samples): the watcher on shift records one
  [run-record §3](watch-run-record.md#3-signal-check-rows) row with
  disposition record-and-watch. No page, no escalation.
- Page-candidate gap (5 quiet samples): the on-call operator owns the
  ack with the standard 15-minute bound from the page, then works §6.
  Unacked past 30 minutes escalates to the watch lead per the
  [ack-owners ladder](watch-ack-owners.md#3-escalation-ladder).
- An incident open at a checkpoint forces that row to EXTEND or
  ROLLBACK — never GO ([run-record §5](watch-run-record.md#5-checkpoint-gono-go-annotations)).

No new rule id, paging path, or escalation level is created here. If a
future change wires this sketch into `wrangler/src/alert-rules.ts`, that
change names the rule, links the [runbook](runbook.md) section, and
updates this file in the same PR.

## 6. First response (investigation only — no control plane change)

1. Confirm the shape with the §3 queries: flat ACKs, flat recovery
   series, fresh keepalive samples (G1) versus stale everything (G2).
2. Read `/readyz`: gateway `starting` with a fresh HELLO storm is a
   reconnect, not a zombie; gateway `ready` with flat ACKs is the zombie.
3. Correlate container stdout for disconnect/reconnect, invalid-session,
   and close-code lines. Do not fetch or paste credentials.
4. This monitor observes only; it does not restart the bot, reset a
   breaker, replay writes, or change Discord permissions/intents. Fix
   through the existing rollback/runbook paths.

## What this spec does not do

- No threshold is set or changed here; the sketches in §2 are wiring
  input, not live rules.
- No alert rule, webhook, scrape job, dashboard, or external monitor is
  added or modified.
- No database probe is authorized against staging or production.
- No retry, breaker-reset, takeover, rollback, or credential step is
  included; those live in the runbook and cutover docs.

## Sources

- ACK/latency semantics: `crates/bot/src/gateway_metrics.rs`,
  `crates/core/src/metrics.rs`, [metrics](metrics.md).
- Session continuity and the zero-missed-events acceptance:
  [watch-signal-queries §8](watch-signal-queries.md#8-gateway-disconnects-and-missed-events).
- Keepalive tick, unready threshold, arm-failure signal:
  [container-readiness](container-readiness.md), [runbook](runbook.md#logs-and-keepalive).
- Event spellings: [observability-event-catalog](observability-event-catalog.md).
- Run-record sheet, ack owners, handover:
  [watch-run-record](watch-run-record.md),
  [watch-ack-owners](watch-ack-owners.md),
  [cutover-watch-handover](cutover-watch-handover.md).
