# Voice cutover rollback triggers (staging)

Rollback side of the voice cutover leaf. The forward steps live in the
rehearsal runbook (`docs/voice-cutover-rehearsal.md`); this document records
**when to roll back** and how to verify the migration gate that guards the
staging path. Staging only; never production.

## 1. Rollback decision triggers

Any single trigger fires the rollback procedure in
[`docs/cutover.md`](cutover.md#rollback-preserve-next-window-writes-before-reopening-legacy).
Check every trigger manually at the watch checkpoints (+15 min, +1 h,
+6 h, +24 h, +48 h). Only the conditions labelled **installed rule** below
are also evaluated by the Worker at keepalive samples; no rule covers all
rollback triggers. A trigger fires on **measured evidence**, never on a
single noisy sample. Alert silence does not replace a manual checkpoint.

### T1. DB-behind-binary (staging rollout-timeout lesson)

The staging container boots with embedded migrations baked into the binary.
When the staging database lags the binary's migration set, boot reads for
tables the database does not have yet fail with
`checkpoint_load_failed`, the container crashloops, and
`deploy-staging verify` times out with `rollout_timeout`. This exact
signature recurred on 2026-10-03 across consecutive main pushes:
`rollout=completed instances=active:0,healthy:1,failed:0,starting:0,scheduling:0`.

- **Trigger:** `deploy-staging verify` fails `rollout_timeout` **and** the
  last observation names `gateway_failure=durable_gateway:checkpoint_load_failed`,
  or the container log names `checkpoint_load_failed` as the fatal class.
- **Response:** do not re-run `deploy-staging` (the same binary against the
  same lagging database fails the same way). Run `staging-migrate plan`
  first, reconcile the pending list against the reviewed cutover plan, then
  `staging-migrate apply` through its governed path. Only then re-deploy.
- **Cutover gate:** the voice swap does not start while any
  `checkpoint_load_failed` receipt is open on staging, because a staging
  container that cannot read its own checkpoint cannot own voice rooms.

### T2. Pending-migration check

`staging-migrate plan` prints `pending_before`: the source versions absent
from the staging ledger, in source order. Any nonempty pending list at
cutover freeze time means the database the bot boots against is **not**
the schema the reviewed binary was tested against.

- **Trigger:** `pending_before` is nonempty at freeze time, or the apply
  run's `expected_pending` (bound to the reviewed plan) no longer equals
  the computed pending list (the runner refuses before any DDL; that
  refusal is itself the trigger).
- **Response:** treat the mismatch as NO-GO for the cutover window.
  Reconcile: ledger drift (checksum/unknown/failed rows) means the staging
  database diverged from the reviewed source and needs a data-lead decision,
  not a blind apply; a pure lag means run the governed apply and re-read
  the pending list to empty before proceeding.

### T3. Error budgets (voice room operations + gateway continuity)

Voice cutover success is measured in rooms, not just process health. The
budgets below use the signals the runtime already emits: `/readyz`
component breakdown, `/metrics` counters, `report voice-reconcile` funnel
halves, and the reconcile pass (tracked-vs-live room comparison).

| Budget | Window | Rollback trigger | Coverage today |
|---|---|---|---|
| Room create/move/delete failures | 15 min rolling | > 5% of creator joins fail to place a room, or any member stranded (joined a creator and landed nowhere) | **Manual checkpoint** for joins/stranding. Installed `voice_failures` measures terminal room-operation failure share between keepalive samples (>5%, minimum 10 operations), not a 15 min creator-join ratio |
| Ghost channels | per reconcile pass | tracked-but-orphaned rooms grow across two consecutive passes, or any untracked channel is deleted by the bot (never allowed) | **Manual checkpoint**; no installed rule reads `two_bot_voice_compensation_pending`. Installed `voice_failures` catches new dead-letters/orphans, not pending-compensation growth or wrong deletion |
| Vote-kick ballots | per ballot | any ballot executes against the wrong member/channel, or duplicate ballot side effects from a retried interaction | **Manual checkpoint**; no installed identity/duplicate-side-effect rule |
| Gateway event gap | 5 min rolling | any unexplained gap in join/message/voice receipts vs independent moderator observation, or duplicated command execution from a replay | **Manual checkpoint** for observation/effects. Installed `gateway_missed_events` catches received-session sequence gaps, not all missing/duplicate effects |
| REST health | per keepalive window | 429 share > 10% with >= 10 requests, or any 5xx/transport burst on voice routes that survives containment | Installed `rest_429_rate` covers the 429 share. **Manual checkpoint** for persistent 5xx/transport failures; no installed REST burn rule |
| Job failures | 3 consecutive completed failures; checkpoint age is event-driven | Three failed completions on a labelled voice-adjacent scheduled job; session checkpoint does not advance despite observed commit-eligible traffic | Installed `job_consecutive_failures` for labelled failed completions. **Manual checkpoint** for `session_checkpoint`: stale-exempt, success-only job instrumentation, no scheduled cadence or installed age/commit-failure rule |

Budgets reset at each watch checkpoint only when the window closed green;
a trigger that fired stays open until its named disposition is recorded.

### Manual checkpoint queries

Use the authenticated Worker [`/ops/metrics` read path](metrics.md#off-container-scrape-and-alert-rules)
for staging. It returns current series, **not query history**; no Prometheus
server is shipped. The PromQL below requires an existing authorized history
store with complete scrapes for the window, scoped to one staging deployment.
Alternatively retain timestamped snapshots at the normal scrape cadence and
compare counters/gauges by hand, splitting counters at resets. Missing series,
missing history, zero denominators and reset-spanning gaps are **unknown**, not
health. The application `job` selectors below assume preserved metric labels;
if the scraper uses Prometheus's default `honor_labels: false`, select
`exported_job="session_checkpoint"` instead of its target's `job` label.

- **Room failures:** inspect terminal operation counts and failed share:

  ```promql
  sum by (op) (increase(two_bot_voice_operations_total{outcome!="success"}[15m]))
  / sum by (op) (increase(two_bot_voice_operations_total[15m]))
  ```

  Container log filter: `voice_event = "voice_operation" AND outcome != "success"`.
  These are operation outcomes, not creator joins; compare moderator-observed
  creator joins with actual room placements to check the >5%/stranded-member
  trigger. The metric/log cannot prove a member was not stranded.

- **Ghost/compensation growth:** inspect `two_bot_voice_compensation_pending`
  and `delta(two_bot_voice_compensation_pending[15m])`; log filters are
  `voice_event = "voice_reconcile"` and `voice_event = "voice_creator_orphan"`.
  This is a gauge, so never use `increase`. It is a last-writer-wins value
  across voice workers, not a per-guild sum; two scrape samples are not two
  completed reconcile passes. Compare the tracked-vs-live room evidence for
  two actual passes and the moderator's deletion evidence. A flat/zero gauge
  cannot certify no ghosts or no wrong deletion.

- **Vote-kick:** inspect start, refusal and terminal outcomes:

  ```promql
  sum by (outcome) (increase(two_bot_voice_vote_kick_total[15m]))
  ```

  Compare the ballot's start/enforcement audit evidence with independent
  Discord member/channel effects. The bounded outcome series deliberately
  has no member/channel/vote identity; no query on it proves correct targets
  or absence of duplicate side effects. Missing audit/effect evidence leaves
  this manual check unknown, even if the counters look normal.

- **Event continuity:** inspect received dispatch counts and sequence gaps:

  ```promql
  sum by (event) (increase(two_bot_gateway_events_total{event=~"GUILD_MEMBER_ADD|MESSAGE_CREATE|VOICE_STATE_UPDATE"}[5m]))
  ```

  ```promql
  sum(increase(two_bot_gateway_missed_events_total[5m]))
  ```

  Compare these with independent moderator observations and effects. Received
  counts include replays; zero sequence gaps do not certify no lost effects
  or duplicate command execution.

- **Voice-route REST failures:** inspect bounded routes shared with voice:

  ```promql
  sum by (route, result) (increase(two_bot_rest_requests_total{route=~"POST /guilds/:guild/channels|PATCH /guilds/:guild/members/:member|DELETE /channels/:channel",result=~"5xx|transport"}[15m]))
  ```

  Correlate with `voice_event = "voice_operation" AND outcome = "discord"`
  and containment observations. These routes also serve non-voice workloads;
  the query does not by itself attribute errors to voice or prove containment.

- **Session checkpoint:** display success age (Unix seconds) and successful
  commit traffic; zero means never succeeded, not an ancient success:

  ```promql
  time() - (two_bot_job_last_success_timestamp_seconds{job="session_checkpoint"} > 0)
  ```

  ```promql
  increase(two_bot_job_runs_total{job="session_checkpoint",outcome="success"}[5m])
  ```

  ```promql
  sum(increase(two_bot_gateway_checkpoint_failures_total[5m]))
  ```

  The age query omits zero timestamps; record zero/missing explicitly. There
  is no periodic checkpoint cadence: durable commits follow eligible gateway
  dispatches, while heartbeat ACKs and presence updates do not require them.
  Only call an unchanged success timestamp a stall when observed commit-eligible
  traffic did not advance the success counter/timestamp. Any commit-failure
  increase is a finding, but **its alert rule is not installed** at this head.
  Do not use the success-only checkpoint job's failure gauge as proof of health.

Record query/log timestamps, coverage gaps and moderator evidence in the watch
checkpoint. None of the manual checks above automatically pages or opens a
ticket; an observed trigger needs a recorded responder disposition.

### T4. Rollback-watermark gap

Per `docs/cutover.md`, maximum accepted loss is **0 acknowledged committed
database writes** across the Next window. `rollback-delta --since T_f`
measures that delta read-only.

- **Trigger:** `rollback-delta` reports any table as `unmeasurable`
  without a reviewed disposition, the export refuses (any table over the
  50,000-row cap, signalling a mis-set `--since`), or the journal capture
  for the window is incomplete.
- **Response:** freeze new writes, keep both data sets, escalate a decision
  brief to the Director of Engineering. Never silently choose data loss to
  restore availability.

### T5. Registry/permission drift

- **Trigger:** final live command definitions or guild permissions at
  rollback freeze differ from the reconciled target with no approved
  watch-window edit covering the drift, or any role allow revoked during
  the watch cannot be mapped to the legacy-compatible target.
- **Response:** keep commands frozen with a named moderator/data-lead
  disposition; unresolved drift goes to the Director of Engineering.

## 2. Migration-gate behavior verification (staging path)

The staging-migrate gate refuses-or-fails loudly by this contract:

1. **Refusal before DDL (exit 2):** bad source SHA, prod-like or pooler
   host/database pins, binding/target mismatch, non-bare evidence or ACL
   refs, missing binding, ledger failed rows / unknown versions / checksum
   drift, and (apply only) `expected_pending` mismatch. No DDL is attempted
   on any of these paths.
2. **Failure with evidence (exit 1):** migration execution errors and
   incomplete ledgers after apply, with `ledger_after_failure` on stdout.
3. **CI amplification (the #406 lesson):** both `plan` and `apply` Run
   steps use `shell: bash` plus `set -o pipefail`, so a piped refusal or
   failure fails the job instead of reporting green through `tee`. Pinned
   by `scripts/ci/test_workflows.py` (`staging_migrate_errors`); verified
   offline on this branch: 13 tests OK.
4. **Observed refusal receipt (staging path, real run):**
   `staging-migrate` run 37134899975 (2026-10-03, `workflow_dispatch`,
   source `8854bfb5`) completed `success` while its log reads
   `staging-migrate refused: reviewed ACL plan reference is missing or not
   a bare reference` — the pre-#406 swallowed refusal, with the non-bare
   ACL ref (a card reference carrying `;` and `@` segments) visible in the same
   log. No post-#406 dispatch exists yet (exactly one `staging-migrate`
   run in history), so live loud-failure confirmation stays open: the
   next real dispatch with a bare ACL ref must go red on a refusal, and
   its job conclusion must equal the migrator's exit class.
5. **Pending-migration cross-check:** the same refused run logged
   `EXPECTED_PENDING` with 32 versions; the reconciler's embedded
   `STAGING_PENDING_AT_BASE` fixture (24 versions at its base SHA) plus
   live-set growth is the unit-level proof the pending computation stays
   correct as the source grows (`staging_ledger_fixture_is_set_based`).

## 3. Open items (not this card's deliverable)

- Live loud-failure confirmation: next real `staging-migrate` dispatch
  after #406 must demonstrate a refusal failing the job (needs the
  Operator/CEO-governed dispatch; agents cannot dispatch it).
- Live staging practice of the §4 create/move/delete probes (blocked on
  staging returning to healthy; owned by the rehearsal follow-up).
- Voice lifecycle outcome signals (`feat/voice-lifecycle-signals`, not on
  main) and the rehearsal doc (`docs/voice-cutover-rehearsal`, not on
  main) are sibling-branch work; the budgets above consume their signals
  but do not depend on their merge.
- Production authorization and sequencing live on the cutover card, not here.
