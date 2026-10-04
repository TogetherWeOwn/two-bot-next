# Production cutover guard: Coolify-warm single-gateway checklist

Companion to the [production cutover procedure](cutover.md) and the
[pre-flight go/no-go sheet](cutover-preflight-checklist.md). Those answer
*whether the candidate may go*; this sheet answers *how the swap itself stays
safe*: the legacy Coolify bot kept warm but stopped, exactly one gateway
session at a time, the stop/start order, and the rollback GO/NO-GO decision.
It is a **procedure, not approval to execute it**. The timed staging backout
drill and the alert-tuning checklist live in their own leaves; this sheet
only names the rollback path they must converge on.

How to use: the cutover lead walks top to bottom during the window, checking
each box only with the named receipt in hand. Any unchecked box is NO-GO.
Host steps run only through the authorized broker or an `Operator:` handoff
with command and rollback — never from an agent shell. Keep member data,
dumps and logs in the restricted evidence location, never in public comments.

## 1. Coolify kept warm (before T_f)

- [ ] Legacy image digest and configuration are pinned on the execution card;
  the pinned pair matches the last healthy legacy deployment record.
- [ ] Legacy auto-deploy is off and restart supervision cannot reconnect it
  during the window; duplicate replicas are confirmed absent.
- [ ] Legacy database recovery points (snapshot IDs/hashes, restore receipt)
  are recorded and their retention exceeds the watch/reconciliation period.
- [ ] The Next Worker/DO ownership fence is armed and already refuses Next
  startup while legacy owns the application, including health callers and
  scheduled keepalive work. A stopped container alone is not a fence.
- [ ] The rollback path is named on the execution card before GO: the
  production Worker rollback dispatch (previous version ID kept in the watch
  header) plus the legacy Coolify restart with the reconciled database
  binding. Whether that path was exercised in rehearsal or waived under the
  epic's authority is recorded — an unexercised, unwaived path is NO-GO.

Source: [cutover.md](cutover.md) (freeze-and-drain, rollback sections);
[production-deploy.md](production-deploy.md) (rollback dispatch, watch header).

## 2. One gateway at a time (session-start budget)

- [ ] Exactly one gateway owner is recorded at every step: legacy stopped,
  Next stopped, or a single named owner — never two live gateways.
- [ ] Every session start is recorded: IDENTIFY versus RESUME, READY versus
  RESUMED, invalid sessions (`d: false`, close codes 4007/4009) and the
  current session-start budget reading. No restart loop may consume the
  recovery reserve; budget exhaustion means waiting for the recorded reset,
  not spin-restarting.
- [ ] First production boot uses the armed one-shot force-fresh IDENTIFY;
  legacy session state is never imported. The follow-up dry run shows the
  directive consumed and a new session sequence.
- [ ] After each start, `/readyz` 200 plus one gateway session is confirmed
  from logs and control-plane receipts — not from a health-200 alone.

Source: [gateway-recovery.md](gateway-recovery.md) (checkpoint contract,
force-fresh IDENTIFY); [production-deploy.md](production-deploy.md) watch log.

## 3. Stop/start order

1. Freeze new bot commands and web/internal-action writes; pause every
   producer, cron and consumer that can mutate the copied data.
2. Drain to zero admitted work (or explicitly preserve and reconcile each
   pending item); record queue depth, in-flight requests and last good jobs.
3. Stop legacy gracefully via Coolify/broker; record its last event and
   terminal state. Confirm nothing can reconnect it. Record `T_f` and final
   writer watermarks.
4. Copy and verify data, then reconcile the command registry, strictly in the
   order the cutover procedure prescribes; start neither gateway while
   verification is unresolved.
5. Release the Next fence and start **one** Next container only after the
   lead confirms every data, registry and permission gate passed. Record
   first READY and the remaining session-start budget.
6. Release producers in the recorded order only after the lead's GO; record
   `T_0` when service is ready and open.

Source: [cutover.md](cutover.md) (freeze-and-drain, data copy, registry swap).

## 4. Rollback GO/NO-GO decision

GO requires all of: no overlapping gateway or writers; final data and
registry checks match; preflight has no FAIL; fresh READY with all required
components healthy; no unexplained event gap, duplicate effect or missed
deadline; all feature sign-offs and rollback receipts present.

Roll back on any of: auth, intent or permission failure; unmapped runtime
behavior; missed moderation action; unexplained data mismatch; duplicate
side effect; failed internal-action contract; crash loop; session budget
exhaustion; or no durable rollback watermark. Maximum accepted loss is
**zero acknowledged committed database writes** — restore-to-`T_f` alone is
not a rollback.

Rollback order: declare rollback and freeze all Next/web writers again —
starting legacy is not the first step. Verify the persisted ownership fence
**before** stopping Next; drain admitted work, stop Next, verify terminal
state. Reconcile every write since baseline (shared database: validate
legacy against the retained additive schema, never down-migrate; separate
database: recover from the `T_f` baseline plus the full captured Next and
web delta). Measure the window read-only first with
[rollback-delta](rollback-delta.md). Reconcile the registry to the approved
target — never a stale baseline reset — and read back every affected guild.
Only then restart one legacy gateway with a fresh session, verify health
and event continuity, and resume producers in order.

Source: [cutover.md](cutover.md) (registry swap, rollback);
[rollback-delta.md](rollback-delta.md) (window measurement);
[production-deploy.md](production-deploy.md) (watch log, thresholds).
