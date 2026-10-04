# Single-gateway IDENTIFY guard checklist (offline cutover reference)

Companion to the [Coolify-warm single-gateway guard checklist](cutover-guard-checklist.md)
(swap safety and rollback decision), [gateway-recovery.md](gateway-recovery.md)
(checkpoint contract and force-fresh IDENTIFY mechanism), and
[cutover.md](cutover.md) (freeze-and-drain order). Those say *what must hold*;
this sheet says *how the cutover lead proves it, step by step, with the named
command or endpoint in hand*. It is a **procedure, not approval to execute
it**. Host steps run only through the authorized broker or an `Operator:`
handoff with command and rollback — never from an agent shell. Reading this
sheet needs no live guild, no token and no deploy; executing it happens inside
the cutover window only.

How to use: walk top to bottom during the window. Check each box only with
the named receipt in hand. Any unchecked box is NO-GO. Record every receipt
on the execution card with `T_f` (freeze time) and `T_0` (first Next ready).

## 1. Stop-Coolify before start-Container order

Legacy must reach terminal stopped state **before** exactly one Next
container starts. A stopped container alone is not a fence: legacy
auto-deploy and restart supervision must be off so nothing can reconnect it
during the window.

- [ ] Producers, crons and consumers that can mutate the copied data are
  frozen; queue depth is drained to zero (or each pending item is explicitly
  preserved for reconcile). Proof: the queue-depth / in-flight / last-good-job
  record on the execution card ([cutover.md](cutover.md), freeze-and-drain).
- [ ] Legacy is stopped gracefully via Coolify/broker. Proof: the broker
  terminal-state receipt plus legacy's last event, recorded with `T_f` and
  the final writer watermarks. Confirm auto-deploy is off and no duplicate
  replica can reconnect.
- [ ] Data copy, verification, command-registry reconcile and permission gates
  all pass with **neither** gateway running. Proof: each gate's sign-off on
  the execution card; no `/readyz` 200 from any bot container exists yet.
- [ ] The Next ownership fence is released and **one** Next container starts,
  only after the lead confirms every gate above. Proof: first READY plus
  `curl -s -o /dev/null -w '%{http_code}' "$CONTAINER_URL/readyz"` returns
  `200` — `/readyz` is 200 only when the gateway is connected, while
  `/health` is process liveness only and proves nothing about the session.
  (`crates/bot/src/server.rs`, [container-readiness.md](container-readiness.md).)

## 2. IDENTIFY-budget note

Every gateway session start spends from Discord's session-start limit; a
restart loop that burns fresh IDENTIFYs can consume the recovery reserve and
leave no budget to reconnect. One session start per clean restart is the norm
(RESUME on a checkpoint at most 15 minutes old; first production boot
IDENTIFYs via the one-shot armed directive in step 3).

- [ ] The current session-start budget reading is recorded before the swap,
  and every start during the window is logged as IDENTIFY versus RESUME and
  READY versus RESUMED, with invalid sessions (`d: false`, close codes
  4007/4009) noted. Proof: the before/after scrape pair from
  [watch-signal-queries.md](watch-signal-queries.md):
  `curl --silent --show-error --max-time 10 -H "Authorization: Bearer ${METRICS_SCRAPE_TOKEN}" "${WORKER_URL}/ops/metrics"`
  into two files, then diff; a rising
  `two_bot_gateway_events_total{event="READY"}` count means fresh sessions
  (IDENTIFY), a rising `two_bot_gateway_reconnects_total` /
  `two_bot_gateway_resumes_total` means continued sessions. Cross-check by
  counting `gateway shard loop started` log lines over the same window.
- [ ] No restart loop is consuming the reserve. A rising READY count without a
  stable session, an invalid-session storm, or a budget reading that no longer
  allows recovery is investigate-and-possibly-roll-back, never spin-restart.
  (Watch row: [production-deploy.md](production-deploy.md) session-start row;
  limit: [Discord session-start limits](https://docs.discord.com/developers/events/gateway#session-start-limit).)

## 3. Proving a new session versus a RESUME (`/readyz` plus dry run)

The first production boot must IDENTIFY with RESUME disabled, even though a
checkpoint up to 15 minutes old would otherwise RESUME. An operator arms a
one-shot directive; the dry run proves what happened. From an operator
checkout (only the `two-bot` binary ships in the image); `TWO_DATABASE_URL`
is the bot database the target Container uses and `--guild` must equal that
Container's configured guild:

- [ ] Pre-arm dry run (writes nothing). Proof command:
  `cargo run -p two-bot-cutover --bin gateway-force-identify --locked -- --guild "$GUILD_ID"`
  prints guild, shard 0, checkpoint age and directive state. Record the
  printed `seq` value — the new-session comparison baseline.
- [ ] Arm the one-shot. Proof command:
  `cargo run -p two-bot-cutover --bin gateway-force-identify --locked -- --guild "$GUILD_ID" --apply --reason "first production boot" --allow-live-guild`
  (the live guild needs `--allow-live-guild`; a mismatched/unset `GUILD_ID`,
  missing `--reason` or unknown flag refuses with exit 2 before connecting).
- [ ] Start the bot, then prove the fresh session with two receipts together:
  `GET /readyz` reports the gateway connected, **and** the dry run shows the
  directive `consumed at ...` plus a checkpoint with a small age whose `seq`
  restarted **below** the pre-arm value. A new session counts from 1; a RESUME
  would continue above it. Either receipt alone is insufficient.
  (Mechanism: [gateway-recovery.md](gateway-recovery.md), force-fresh IDENTIFY.)

## 4. Residual-window re-arm rule (consumed without READY)

The directive is consumed when the boot read commits. If the process dies
before the discard and READY are committed, a restart within 15 minutes could
RESUME the old checkpoint — the window between consumption and proof.

- [ ] After **every** armed boot, re-run the dry run. If the directive shows
  consumed but **no new READY checkpoint was recorded**, arm again before the
  next start. Re-arming after consumption arms a new one-shot; arming while
  one is pending leaves it unchanged (`already armed`).
- [ ] Never start the next boot on a consumed-without-READY state without
  re-arming: that next boot is the one that would silently RESUME the stale
  session the directive was meant to discard.

## Sources

- [cutover.md](cutover.md) (freeze-and-drain, registry swap, rollback).
- [cutover-guard-checklist.md](cutover-guard-checklist.md) (Coolify-warm, owner record, rollback GO/NO-GO).
- [gateway-recovery.md](gateway-recovery.md) (checkpoint contract, force-fresh IDENTIFY, residual window).
- [production-deploy.md](production-deploy.md) (watch log, session-start row).
- [watch-signal-queries.md](watch-signal-queries.md) (metrics scrape and session-start queries).
- [container-readiness.md](container-readiness.md) (`/readyz` vs `/health`, keepalive sampling).
- [Discord gateway session-start limits](https://docs.discord.com/developers/events/gateway#session-start-limit).
