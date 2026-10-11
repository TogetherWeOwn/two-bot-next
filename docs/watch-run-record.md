# 48h watch run-record sheet (offline form)

Blank form the watcher fills in during the live 48-hour post-cutover watch.
Offline only: this file installs no monitor, adds no alert rule, sets no
threshold, touches no staging or production system, and carries no secret,
token, URL, member data, or human contact. Copy its tables onto the execution
card at `T_0` and fill them there; this file stays blank in the repo.

Distinct from the smoke sheet: the [smoke run-record](smoke-run-record.md)
covers the top-5 live-guild commands; this sheet covers the 48-hour watch
itself (shifts, signal checks, escalations, go/no-go).

Companion docs (normative on conflict — this sheet never overrides them):

- Alert inventory, read paths, paging path, pre-watch check:
  [watch-checklist.md](watch-checklist.md).
- One read-only query per signal: [watch-signal-queries.md](watch-signal-queries.md).
- Who acks each alert, ack bounds, escalation ladder:
  [watch-ack-owners.md](watch-ack-owners.md).
- External check for a missing Worker/alarm: [watch-external-uptime.md](watch-external-uptime.md).
- Watch header, rows, budgets (thresholds live here):
  [production-deploy.md](production-deploy.md#48-hour-watch-log-tog-9699).
- Handover checklist, paging chain, window boundaries:
  [cutover-watch-handover.md](cutover-watch-handover.md).
- Rollback decider, triggers, ordered steps:
  [cutover-rollback-runbook.md](cutover-rollback-runbook.md).

## 0. How to use this sheet

1. At `T_0` (first `/readyz` 200 on the production revision) the cutover
   executor copies §1 onto the execution card and fills it once.
2. The watcher on shift appends one §3 row per finding (record findings,
   not every healthy poll) and one §2 entry per shift change.
3. Every page gets one §4 escalation row with a named ack owner and ack
   time. Every checkpoint gets one §5 go/no-go row.
4. At +48 h the lead fills §6 sign-off. A failed watch keeps the execution
   card open; nothing is retired on the basis of this sheet.

## 1. Watch header (fill once at `T_0`)

| Field | Value |
| --- | --- |
| `T_0` (UTC, ISO-8601) | _fill in_ |
| Watch deadline (`T_0 + 48 h`) | _fill in_ |
| Deployed commit SHA | _fill in_ |
| New Worker version ID | _fill in_ |
| Previous Worker version ID (the rollback command's version ID) | _fill in_ |
| Watch lead (role; names live on the execution card only) | _fill in on card_ |
| Operator rotation contact (on card only) | _fill in on card_ |
| External monitor name (on card only) | _fill in on card_ |
| Pre-watch provisioning receipt (checklist §4 + external-uptime §provisioning) | _link receipt on card_ |

## 2. Shift roster and handover entries

One row per shift. The outgoing watcher fills every column; the incoming
watcher replies with acceptance before the outgoing watcher stands down.
No cell may say "same as last time". Shift changes never move `T_0` or the
deadline.

| Shift | Outgoing watcher | Incoming watcher | Handover (UTC) | Accepted (UTC + name) | Open findings carried | Pending dispositions + owner | Gateway/session state | Ownership fence state | Rollback readiness | Next actor + action |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | | | | | | | | | runbook linked; prev version ID in §1: y/n | |
| 2 | | | | | | | | | unchanged since last checkpoint: y/n | |
| 3 | | | | | | | | | | |
| 4 | | | | | | | | | | |
| _add rows_ | | | | | | | | | | |

Column notes:

- Open findings carried: one line each — UTC time, signal, observation,
  disposition (record-and-watch vs paged).
- Pending dispositions: every overdue sanction, uncertain send, wedged lane
  with its named owner, or "none pending".
- Gateway/session state: last READY/RESUME, reconnect count, session-start
  budget remaining.
- Ownership fence state: which revision holds the active record; confirm no
  takeover/fence change is in flight.
- Rollback readiness: pointer to the rollback runbook + confirmation the
  previous version ID is still in §1.

## 3. Signal-check rows

One row per finding and per checkpoint finding. Healthy polls stay in
bounded restricted evidence, not the thread. Fixed error-class vocabulary
only (never raw text); warn/page values live in the companion docs.

| UTC time | `T_0` offset | Signal / alert | Panel / query source | Observed | Disposition | Ack owner | Ack time (UTC) | Evidence link |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| | | `readyz` / `revision` | checklist §2; queries pack intro | status + wired components (`process`, `gateway`); compiled revision/build-ID match | record-and-watch / paged | watcher on shift | | |
| | | `gateway` session | queries §1 + §8 | IDENTIFY / RESUME / READY / RESUMED / invalid session / close code + session-start budget | | watcher on shift | | |
| | | `gateway` disconnects + missed events | queries §8 | disconnect count + missed-events count (pair; any nonzero missed events fails zero-gap acceptance) | | watcher on shift | | |
| | | `keepalive-gap` (heartbeat ACK inactivity) | [keepalive-gap detector §2–4](keepalive-gap-detector.md#2-proposed-thresholds-and-mandatory-eligibility) | adjacent ACK/recovery deltas + readiness/process eligibility + complete timestamp coverage (2 intervals/120 s = watch; 5/300 s = page candidate); scrape availability separate; tick liveness unknown without positive evidence | record-and-watch / operator investigation; paged only through existing path | watcher on shift / on-call operator | | |
| | | `rest_429_rate` / 5xx share | queries §2; checklist alert `rest_429_rate` | 429 share, hot route, 5xx top routes | | on-call operator | | |
| | | handler latency | queries §3 | p50 / p95 / p99 + `_count` trend (first 24 h vs second 24 h) | | watcher on shift | | |
| | | unban-queue proxies | queries §4 | `periodic job failed` lines + `audit_retry` streak + mirror deliveries (table counts are test-copy only) | | watcher on shift | | |
| | | restarts | queries §5 | `listening` count + `container started/stopped` + counter-reset noted (reset = restart, not quiet) | | watcher on shift | | |
| | | `db_errors` by `op` | queries §6; checklist alert `db_errors` | `admission` vs `other` split | | on-call operator | | |
| | | send-admission decisions | queries §7; checklist alert `send_admission_blocked` | `blocked` share + `storage_error` count (same outage as `db_errors`, not a second one) | | on-call operator | | |
| | | `job_stale:<job>` | checklist; runbook job sections | job name + last-success age vs 2 x cadence | | on-call operator | | |
| | | `job_consecutive_failures:<job>` | checklist; runbook job sections | job name + 3-in-a-row evidence | | on-call operator | | |
| | | `db_pool_saturated` | checklist; runbook pool section | max + zero idle x3 samples | | on-call operator | | |
| | | `container_unready_alert` / `_recovery` | checklist §3; container-readiness runbook | 10 consecutive failing samples / first ready sample after alert | | on-call operator | | |
| | | `container_keepalive_arm_failed` | checklist §3 | monitoring outage, not a readiness sample | | watch lead | | |
| | | `container_unready_webhook_failed` | checklist §3 | type + HTTP status only; fall back to transition log line | | watch lead | | |
| | | `external_health` | external-uptime sheet | non-2xx / timeout / TLS-DNS x10 consecutive | | on-call operator | | |
| | | `error-class` | production-deploy watch log | one fixed class only, no raw text | | watcher on shift | | |
| | | expected-down note | external-uptime §expected-down | deploy / ownership-changeover single samples with deployment id (note, not incident) | | watcher on shift | | |
| | | `rollback-decision` | §5 of this sheet | GO / EXTEND / ROLLBACK + version-ID record | | cutover lead | | |

## 4. Escalation log

Levels follow the [handover sheet](cutover-watch-handover.md#4-escalation-path)
and the [ack-owners ladder](watch-ack-owners.md#3-escalation-ladder).
An incident open at a checkpoint forces that row to EXTEND or ROLLBACK —
never GO.

| UTC paged | Trigger signal + panel/row | Level | Who paged → who | Channel / thread | Ack owner | Ack time (UTC) | Outcome + linked incident |
| --- | --- | --- | --- | --- | --- | --- | --- |
| | | L0 record / L1 page lead / L2 freeze + investigate / L3 rollback-or-extend | watcher → cutover lead; lead → data/moderation lead; lead → Director (brief) | | | ack bound 15 min; escalates to lead at 30 min unacked | contained / EXTEND + new deadline / ROLLBACK + UTC + reason |
| | | | | | | | |
| _add rows_ | | | | | | | |

Rules the log enforces: the watcher pages the cutover lead (never the data
or moderation lead directly); a `RESOLVED` without its alert means the page
was lost — investigate; writers freeze through the reviewed maintenance
mechanism only (no restart-to-free-the-pool, no blind replay, no second
gateway, no credential substitution).

## 5. Checkpoint go/no-go annotations

Checkpoints sit at +15 min, +1 h, +6 h, +24 h, +48 h from `T_0`. One row
each on the execution card. The ``readyz`` + revision match cell comes from
`scripts/cutover_watch_checkpoint.py` output (one call per checkpoint — see
the [48-hour watch log](production-deploy.md#48-hour-watch-log-tog-9699)),
not hand-filled; the verdict, decider, and deadline cells stay human.

| Checkpoint | UTC time | `readyz` + revision match | Open incidents (name or none) | Verdict | Decider | New deadline (EXTEND only) |
| --- | --- | --- | --- | --- | --- | --- |
| +15 min | | | | GO / EXTEND / ROLLBACK | cutover lead | |
| +1 h | | | | GO / EXTEND / ROLLBACK | cutover lead | |
| +6 h | | | | GO / EXTEND / ROLLBACK | cutover lead | |
| +24 h | | | | GO / EXTEND / ROLLBACK | cutover lead | |
| +48 h | | sign-off or extend | | GO (sign-off) / EXTEND | cutover lead | |

A missed budget at any level is an incident, never permission to skip
reconciliation.

## 6. Close-out and sign-off

| Field | Value |
| --- | --- |
| Final verdict (sign-off / extended to _deadline_) | _fill in_ |
| Legacy image, config, recovery points still warm (failed/extended watch) | y / n |
| Nothing retired at watch close on the basis of this sheet | confirmed |
| Follow-up cards (public issue refs or plain descriptions; no secrets) | _fill in_ |
| Lead signature (role) + date/time (UTC) | _fill in on card_ |

## 7. Collection bounds

- Owner fields name roles on this sheet; human names, rotation handles,
  URLs, secrets and channel names live on the execution card only, never
  in the repo.
- No tokens, secrets, private URLs, internal tracker IDs, member/channel/
  message IDs, probe bodies, or raw error text in any filled copy that
  returns to the repo. Failed-auth details name the expected credential
  and the exact error only.
- A missing `T_0`, missing revision match, missing ack time, or missing
  checkpoint row is a gap in the watch, not a silent waiver.
- A counter that reset to zero means the process restarted; it does not
  mean the window was quiet — re-baseline both scrapes after it.
- When this sheet disagrees with a threshold, query, or rollback step in
  a companion doc, the companion doc wins and this sheet is the one to fix.

## Sources

- Alert inventory and paging path: `docs/watch-checklist.md`.
- Read-only queries (one per signal): `docs/watch-signal-queries.md`.
- Ack owners and escalation ladder: `docs/watch-ack-owners.md`.
- External uptime check: `docs/watch-external-uptime.md`.
- Watch header, rows, budgets: `docs/production-deploy.md` (48-hour watch log).
- Handover checklist and escalation path: `docs/cutover-watch-handover.md`.
- Rollback decider and steps: `docs/cutover-rollback-runbook.md`.
