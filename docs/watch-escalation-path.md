# 48h watch escalation path: severity to role to evidence (offline doc)

Status: offline reference. This file installs no monitor, alert rule, scrape
job, dashboard, or webhook, and it changes no threshold, query, or rollback
step. It maps each watch severity to the role that owns it, the channel used,
and the evidence packet attached, so the watcher never has to decide whom to
wake mid-incident. When this file and a live procedure disagree, the live
procedure wins and this file is the one to fix.

Scope: severity tiers (page vs ticket), escalation chain by role, evidence
packet per tier. Read-only; never run probes or queries against staging or
production databases from an agent workspace, and never print secret values,
tokens, connection strings, or member data. This page executes nothing:
every production step stays on the separately authorized execution sheet.

Consumes without repeating:

- Shift mechanics, handover checklist, window boundaries, and L0-L3 duties:
  [48-hour watch handover](cutover-watch-handover.md).
- Alert inventory, paging path, dry-run receipt, and known gaps:
  [watch checklist](watch-checklist.md).
- Per-alert ack owner and ack bounds (15 min, 30 min unacked escalates):
  [watch ack owners](watch-ack-owners.md).
- Panels, warn/page thresholds, and budgets:
  [cutover dashboard queries](cutover-dashboard-queries.md).
- One read-only query per signal:
  [watch signal queries](watch-signal-queries.md).
- Window signals, freeze/drain, and rollback procedure:
  [cutover](cutover.md) (§48-hour watch, rollback section).
- Watch log header, rows, and budgets:
  [production deploy](production-deploy.md) (§48-hour watch log).
- Decider, triggers, ordered steps, and time bounds:
  [cutover rollback runbook](cutover-rollback-runbook.md).

Non-goals: no monitor provisioning, no threshold or query change, no rollback
procedure of its own, no run-record sheet of its own, no live drill.

## 1. Severity tiers

Two tiers only. The threshold numbers live in the dashboard queries; this
table only says what each tier means and how fast a human moves.

| Tier | Meaning | Example signals | Human response |
|---|---|---|---|
| SEV-1 page | Budget-burning or safety-threatening now | Sustained error bleed both windows over burn rate; gateway session loss (unpaired disconnect, missed-event increase, session-start budget exhausted); pool pressure or job failures persisting across a checkpoint; irreconcilable data/registry drift; any rollback trigger from the runbook | Wake the owner role at once; freeze writers; work the runbook |
| SEV-2 ticket | Degraded but bounded, watch closely | Single-window breach that the confirming window does not sustain; warn-level observation from the dashboard queries; one overdue sanction with a named disposition and a near-term due time; slow resource drift inside placement caps | Record a watch row and a ticket; triage within the shift; escalate to SEV-1 if half the watch error budget is spent or the signal persists across two consecutive long windows |

A missed budget at any tier is an incident, never permission to skip
reconciliation. Silence a page only with a recorded decision on the
execution card.

## 2. Escalation chain by role

Roles follow [cutover](cutover.md) (roles, safety, and evidence). The
[ack owners sheet](watch-ack-owners.md) uses the on-shift vocabulary
(on-call operator, watch lead) for the same people; both name roles, never
a named individual and never the owner user, so the chain works whoever
holds the role that night.

| Step | From (role) | To (role) | Channel |
|---|---|---|---|
| 1 | Watcher on shift | Cutover lead | Private moderator/ops channel |
| 2 | Cutover lead | Data lead (DB-side cause) or moderation lead (sanction/event-continuity gap) | Same thread, named handoff |
| 3 | Cutover lead | Director of Engineering, as a decision brief | Execution card |
| 4 | Cutover lead | Authorized executor through the existing `Operator:` handoff, one per step (host, secret, or staging-DB step) | Execution card |

Rules: the watcher never pages past the cutover lead; a handoff names the
receiving role and the exact signal, never "take a look"; if the cutover
lead is unreachable, the watcher freezes writers, holds the window, and
escalates to the Director of Engineering. No second gateway, no blind replay,
no credential substitution while waiting for an answer.

## 3. Evidence packet per tier

Attach the packet where the tier lives: SEV-1 in the private ops channel
and on the execution card; SEV-2 as a watch row plus the ticket. Raw healthy
polls stay in bounded restricted evidence, not the thread.

SEV-1 packet:

- UTC time, `T_0` offset, and window position (next checkpoint, time left).
- Signal in one line: panel or query name, observed value, threshold crossed.
- Watch header pointer (deployed commit, new/previous Worker version IDs).
- Run-record row link in the 48-hour watch log (or "row opened at HH:MM").
- Named next actor and next action with a time bound.

SEV-2 packet: the same five lines, minus the execution-card copy; the
watch row plus the ticket number closes the loop. Re-firing after reset
opens a new ticket; it never reopens a closed one.

## 4. Companions

- Run-record sheet: the 48-hour watch log in
  [production deploy](production-deploy.md) owns the rows this path points
  at. This file defines the pointer, not the log.
- Alert drill: the offline synthetic paging exercise already ran and its
  receipt lives in the [watch checklist](watch-checklist.md) (§5 paging
  dry-run receipt). The live end-to-end page on staging is still open as
  an operator-gated follow-up on the cutover thread. A drill finding that
  changes a threshold lands in the dashboard queries first; this path only
  re-points if a role or channel changes.

## 5. What this doc does not do

- No monitor, webhook, scrape job, or dashboard is added or configured.
- No threshold is set or changed; warn/page values live in the dashboard
  queries and the deploy watch budgets.
- No rollback step is included; decider, triggers, ordered steps, and time
  bounds live in the rollback runbook and cutover procedure.
- No database probe is authorized against staging or production.
- No retry, breaker-reset, takeover, rollback, or credential step is
  included; those live in the runbook and cutover docs.
