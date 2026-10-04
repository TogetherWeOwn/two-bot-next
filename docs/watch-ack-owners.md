# 48h watch: ack owners and escalation path

Companion to the [alert checklist](watch-checklist.md). The checklist names
the alerts, the read paths and the paging path; this sheet names who
acknowledges each alert, how fast, and what happens when they do not. It
installs no monitor, adds no alert rule, and changes no threshold.
Threshold budgets live in
[production-deploy.md](production-deploy.md#signal-thresholds-budgets-and-rollback-triggers)
and [cutover.md](cutover.md#48-hour-watch); when this sheet disagrees with
those, those win and this sheet is the one to fix.

Roles only: no human name, rotation handle, URL, secret or channel name
goes in this repo. The per-watch named coverage (watch lead, operator
rotation contact, monitor name) is recorded on the execution card, per the
[external uptime sheet](watch-external-uptime.md#provisioning-check-names-only-never-values).

## 1. Ack ownership matrix

To acknowledge a page means: respond per the linked runbook section,
record the finding on the watch log with a named disposition, and confirm
the `RESOLVED` line before closing the incident
([checklist §3](watch-checklist.md#3-paging-path)).

| Alert | Ack owner | Ack bound |
|---|---|---|
| `container_unready_alert` | on-call operator | 15 min from the page |
| `container_unready_recovery` | on-call operator (closes the paired incident) | with the paired alert's row |
| `container_keepalive_arm_failed` | watch lead — the paging path itself is broken | 15 min; treat as a monitoring outage, not a readiness sample |
| `container_unready_webhook_failed` | watch lead — the page may have been lost | 15 min; fall back to the transition log line as the primary signal |
| `external_health` (external monitor) | on-call operator, via the independent channel | 15 min from the external page |
| `job_stale:<job>` | on-call operator | 15 min from the page |
| `job_consecutive_failures:<job>` | on-call operator | 15 min from the page |
| `rest_429_rate` | on-call operator | 15 min from the page |
| `db_pool_saturated` | on-call operator | 15 min from the page |
| `db_errors` | on-call operator | 15 min from the page |
| `send_admission_blocked` | on-call operator | 15 min from the page |
| `voice_failures` | on-call operator | 15 min from the page |

Log-only findings (gateway session starts, handler-latency quantiles,
unban-queue depth, restart counts) need no ack; they are recorded on the
watch log as observations. See the [query pack](watch-signal-queries.md).

## 2. Ack bounds and what missing one means

- **15 min:** every paged alert is acknowledged within 15 minutes. The
  bound matches the first watch checkpoint (`+15 min`), so an unacked
  page is already visible at the earliest GO / EXTEND / ROLLBACK row.
- **30 min unacked:** the page escalates to the watch lead (ladder L2).
  The lead either acks it or re-pages the rotation; the miss is recorded
  on the watch log as a coverage gap, not as a quiet window.
- **Next checkpoint still open:** any incident open at a checkpoint
  forces that row to EXTEND or ROLLBACK — never GO. A page that ages
  past a checkpoint without a named disposition fails the watch.

## 3. Escalation ladder

- **L1 — on-call operator.** First response per the linked runbook
  section ([runbook](runbook.md#alert-rest-429) for 429,
  [runbook](runbook.md#alert-db-pool) for pool, job sections for
  streaks/staleness, [container
  readiness](container-readiness.md#responding-to-an-alert) for unready).
  Contain through the affected writer's verified control; freeze writers
  when the threshold table says so.
- **L2 — watch lead.** Enter when a page is unacked past 30 min, when a
  monitoring-outage signal fires (`container_keepalive_arm_failed`,
  unpaired `RESOLVED`, missing `container_unready_recovery`), when
  several alerts coincide (pool pressure plus job failures plus gateway
  `starting`/`down`), or when the fix belongs to a dependency owner
  (for example the Neon dashboard shows trouble on the staging branch).
  The lead owns the incident to checkpoint resolution and calls the
  EXTEND / ROLLBACK row.
- **L3 — rollback dispatch with a bounded decision brief.** Enter when a
  rollback-trigger value from the threshold table is reached, or when L2
  cannot identify a recovery path by the checkpoint. The brief goes to
  the Director of Engineering; rollback then follows
  [cutover.md](cutover.md#rollback-preserve-next-window-writes-before-reopening-legacy)
  with maximum accepted loss of zero acknowledged committed writes.
  Rollback is a stop-the-bleed decision, not a diagnosis; the incident
  record stays open on the execution card.

## 4. Watch handoff record (on the execution card, never here)

Before `T_0` the lead records: the watch lead for this window, the
operator rotation contact, the external monitor name, and the monitoring
receipts from the [checklist pre-watch
check](watch-checklist.md#4-pre-watch-provisioning-check-names-only-never-values).
At each checkpoint (`+15 min`, `+1 h`, `+6 h`, `+24 h`, `+48 h`) the lead
records one GO / EXTEND / ROLLBACK row with open incidents listed by
name. At `+48 h` record sign-off or extend the watch on the execution
card.

## What this sheet does not do

- No human name, contact, URL, secret or channel name is recorded here.
- No threshold is set or changed here.
- No alert rule, webhook, scrape job, dashboard or external monitor is
  added here.
- No live or staging system is touched by reading this sheet.
