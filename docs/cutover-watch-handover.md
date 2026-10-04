# 48-hour watch handover and paging escalation (offline doc)

Status: offline reference. This file installs no monitor, alert rule, scrape
job, dashboard, or webhook, and it changes no threshold, query, or rollback
step. It says who watches the production cutover window, what the outgoing
watcher hands to the incoming watcher, who pages whom, and where the
`T_0` / `T_0 + 48 h` boundaries sit. When this file and a live procedure
disagree, the live procedure wins and this file is the one to fix.

Scope: handover checklist, paging chain, escalation levels, window
boundaries. Read-only; never run probes or queries against staging or
production databases from an agent workspace, and never print secret values,
tokens, connection strings, or member data. This page executes nothing:
every production step stays on the separately authorized execution sheet.

Consumes without repeating:

- Query panels and warn/page thresholds:
  [cutover dashboard queries](cutover-dashboard-queries.md).
- Rollback decider, triggers, ordered steps, and time bounds:
  [cutover rollback runbook](cutover-rollback-runbook.md).
- Window signals, freeze/drain, and rollback procedure:
  [cutover](cutover.md) (§48-hour watch, rollback section).
- Watch header, rows, and budgets:
  [production deploy](production-deploy.md#48-hour-watch-log-tog-9699).
- Operational detail channel format:
  [cutover](cutover.md#communication-template).

Non-goals: no monitor provisioning, no incident-playbook rewrite, no new
threshold or query, no rollback procedure of its own.

## 1. Window boundaries

- `T_0` is the first `/readyz` 200 on the production revision. The cutover
  executor records it in UTC (ISO-8601) in the watch header on the execution
  card, with the deployed commit, the new and previous Worker version IDs,
  and the named coverage for the whole watch.
- The watch deadline is `T_0 + 48 h`. Checkpoints sit at +15 min, +1 h,
  +6 h, +24 h, and +48 h. Each checkpoint records GO, EXTEND (with a new
  deadline), or ROLLBACK on the execution card.
- At +48 h the lead records sign-off or extends the watch on the execution
  card. A failed watch keeps the execution card open and the legacy image,
  configuration, and recovery points warm; nothing is retired at watch
  close on the basis of this file.
- Shift changes never move a boundary: handover transfers responsibility
  inside the window, it does not reset `T_0` or the deadline.

## 2. Who pages whom

Roles follow [cutover](cutover.md#roles-safety-and-evidence): the cutover
lead owns go/no-go, timing, the watch, and rollback; the data lead owns
copy, verification, and reverse reconciliation; the moderation lead owns
outstanding sanctions and the moderator record; the executor runs authorized
deployment/data steps; the Director of Engineering resolves technical
decisions. Missing evidence means NO-GO at every level below.

| Situation | Who pages | Who is paged | Channel |
|---|---|---|---|
| Any page-level signal from the dashboard queries, or any checkpoint disagreement | Watcher on shift | Cutover lead | Private moderator/ops channel |
| DB-side cause (pool pressure, job failures, store/checkpoint class persists) | Cutover lead | Data lead | Same thread, named handoff |
| Missed deadline, overdue sanction, or event-continuity gap vs the moderator record | Cutover lead | Moderation lead | Same thread, named handoff |
| Rollback decision, irreconcilable data/registry drift, or any proposed loss | Cutover lead | Director of Engineering, as a decision brief | Execution card |
| Host, secret, or staging-DB step | Cutover lead | Authorized executor through the existing `Operator:` handoff, one per step | Execution card |

The watcher never pages the data or moderation lead directly past the
cutover lead, and never starts a second gateway, replays an uncertain
Discord effect, or substitutes a credential while waiting for a page to
be answered. If the cutover lead is unreachable, the watcher freezes
writers, holds the window, and escalates to the Director of Engineering.

## 3. Shift handover checklist

The outgoing watcher copies this list into the watch thread and fills
every line. The incoming watcher replies with acceptance before the
outgoing watcher stands down. No line may say "same as last time".

- [ ] Current window position: `T_0`, now (UTC), next checkpoint and time
  remaining to the `T_0 + 48 h` deadline.
- [ ] Watch header location on the execution card (deployed commit,
  new/previous Worker version IDs, named coverage) and confirmation it
  is unchanged since the last checkpoint.
- [ ] Open findings since the last handover: one line each with UTC time,
  signal, observation, and disposition (record-and-watch vs paged).
- [ ] Pending dispositions: every overdue sanction, uncertain send, and
  wedged lane with its named owner, or "none pending".
- [ ] Gateway/session state: last READY/RESUME, reconnect count, and
  session-start budget remaining.
- [ ] Ownership fence state: which revision holds the active record, and
  confirmation no takeover/fence change is in flight.
- [ ] Rollback readiness: pointer to the rollback runbook and
  confirmation the previous version ID is still in the watch header.
- [ ] Next actor and next action: who does what, by when, if the next
  checkpoint or the next finding arrives first.

## 4. Escalation path

Levels are cumulative: each level keeps the duties of the ones below it.
Threshold values live in the dashboard queries and the deploy watch
budgets; this path only says who acts.

1. **L0 — Record and keep watching (watcher).** A warn-level observation:
   log it as a watch row with UTC time, `T_0` offset, signal,
   observation, and disposition. Raw healthy polls stay in bounded
   restricted evidence, not the thread.
2. **L1 — Page the cutover lead (watcher).** Any page-level signal, any
   unpaired disconnect, any missed-event increase, or any checkpoint that
   cannot record GO. The watcher states the signal, the window position,
   and the exact panel/row observed, then holds.
3. **L2 — Freeze writers and investigate (cutover lead).** Stop new bot
   commands and web/internal-action writes through the reviewed
   maintenance mechanism; drain admitted work to zero or reconcile each
   pending item explicitly. Pull in the data or moderation lead per the
   table above. No restart-to-free-the-pool, no blind replay, no second
   gateway.
4. **L3 — Decide rollback or extend (cutover lead with the Director of
   Engineering).** If no recovery path is identified by the checkpoint,
   the lead declares rollback with UTC time and reason and works the
   rollback runbook in document order. Any proposed loss or
   irreversible recovery needs its separate authority; the lead cannot
   waive the zero-loss gate. If the window stays held, record EXTEND
   with the new deadline.

A missed budget at any level is an incident, never permission to skip
reconciliation.

## 5. What this doc does not do

- No monitor, webhook, scrape job, or dashboard is added or configured.
- No threshold is set or changed; warn/page values live in the dashboard
  queries and the deploy watch budgets.
- No rollback step is included; decider, triggers, ordered steps, and
  time bounds live in the rollback runbook and cutover procedure.
- No database probe is authorized against staging or production.
- No retry, breaker-reset, takeover, rollback, or credential step is
  included; those live in the runbook and cutover docs.
