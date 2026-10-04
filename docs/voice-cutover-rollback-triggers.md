# Voice cutover rollback triggers (staging)

Rollback side of the voice cutover leaf. The forward steps live in the
rehearsal runbook (`docs/voice-cutover-rehearsal.md`); this document records
**when to roll back** and how to verify the migration gate that guards the
staging path. Staging only; never production.

## 1. Rollback decision triggers

Any single trigger fires the rollback procedure in
[`docs/cutover.md`](cutover.md#rollback-preserve-next-window-writes-before-reopening-legacy).
Triggers are checked at the watch checkpoints (+15 min, +1 h, +6 h, +24 h,
+48 h) and continuously by the alert rules below. A trigger fires on
**measured evidence**, never on a single noisy sample.

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

| Budget | Window | Rollback trigger |
|---|---|---|
| Room create/move/delete failures | 15 min rolling | > 5% of creator joins fail to place a room, or any member stranded (joined a creator and landed nowhere) |
| Ghost channels | per reconcile pass | tracked-but-orphaned rooms grow across two consecutive passes, or any untracked channel is deleted by the bot (never allowed) |
| Vote-kick ballots | per ballot | any ballot executes against the wrong member/channel, or duplicate ballot side effects from a retried interaction |
| Gateway event gap | 5 min rolling | any unexplained gap in join/message/voice receipts vs independent moderator observation, or duplicated command execution from a replay |
| REST health | per keepalive window | 429 share > 10% with >= 10 requests (existing `rest_429_rate` rule), or any 5xx/transport burst on voice routes that survives containment |
| Job failures | 3 consecutive | `job_consecutive_failures >= 3` on any voice-adjacent job (existing rule); stale `session_checkpoint` success timestamp across two cadences |

Budgets reset at each watch checkpoint only when the window closed green;
a trigger that fired stays open until its named disposition is recorded.

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
