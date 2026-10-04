# Voice rollback-trigger tabletop drill record (2026-10-04)

Offline tabletop walk of `docs/voice-cutover-rollback-triggers.md` triggers
T1–T5 against the T0 fixture
(`docs/t0-acceptance-smoke-contract.json`, schema version 1) plus recent
public staging CI metadata and merged source. No deploy, no production
contact, no secrets, no live-guild writes. Drill head `9f0835e9`.

Companion documents: the forward rehearsal steps
(`docs/voice-cutover-rehearsal.md`), the lifecycle alert drill record
(`docs/voice-lifecycle-alert-drill.md`, PASS on log query / NEEDS WORK on
rule coverage), and the ghost-count staging verification inside the
rehearsal record (NEEDS WORK, 2026-10-03).

## 1. Verdict table

| Trigger | Verdict | Deciding evidence |
|---|---|---|
| T1 DB-behind-binary | NO-FIRE | `deploy-staging` runs `37166456262` (`rollout_timeout`, `active:1,healthy:0`) and `37166919319` (`api_transport_failure`); neither last observation names `checkpoint_load_failed` |
| T2 Pending-migration check | NO-FIRE | No cutover freeze declared; last `staging-migrate` run (`37134899975`, 2026-10-03) refused before DDL on a non-bare ACL ref, so no freeze-time `pending_before` exists and no post-gate `expected_pending` mismatch refusal occurred |
| T3 Error budgets (live windows) | NO-DATA | No room/ballot/gateway/REST/job window ever observed on staging; no alert rule reads any `two_bot_voice_*` series (proven by the lifecycle alert drill §2/Q4 synthetic exposition: `FIRING-VOICE-FAILURES: []`) |
| T4 Rollback-watermark gap | NO-DATA | 13 voice tables are `Unmeasurable` in `rollback_delta.rs` `TABLE_SPECS` with no reviewed disposition on record; no Next window measured |
| T5 Registry/permission drift | NO-DATA | No freeze-time live-definition/permissions snapshot plus reconciled-target pair exists to compare |

## 2. T3 sub-budget detail

| Budget | Verdict | Evidence / owner |
|---|---|---|
| Room create/move/delete failures, 15 min rolling | NO-DATA | No live room ever created on staging (rehearsal §5, 2026-10-03: staging `/readyz` 503, deploys red). Live run filed as its own gap card. |
| Ghost channels, per reconcile pass | NO-DATA | Ghost count unverifiable: `voice_rooms` absent on the staging DB, `untracked_present=[3]`, `clean=false`, no residual baseline. Same live-run gap card; ghost sweep after cleanup stays with its existing owner. |
| Vote-kick ballots, per ballot | NO-DATA | No ballot ever executed on staging. Execution belongs to the existing staging smoke covering creator create, join/move, kick ballot and cleanup; no new card (no duplicate scope). |
| Gateway event gap, 5 min rolling | NO-DATA | No observation window; staging gateway not healthy. Capture belongs to the same healthy-staging run (record join/message/voice receipts vs observation); continuous alerting belongs to the 48 h watch. |
| REST health, per keepalive | NO-DATA | No keepalive window on voice routes. The `rest_429_rate` rule exists in `wrangler/src/alert-rules.ts`; voice-route bursts unobserved. Same healthy-staging run. |
| Job failures, 3 consecutive | NO-FIRE | `job_consecutive_failures >= 3` covers any voice-adjacent job by name; no streak evidence in available CI metadata. Live two-cadence `session_checkpoint` staleness check belongs to the healthy-staging run. |

## 3. Per-trigger notes

1. **T1.** The trigger needs `rollout_timeout` **plus** a last observation
   naming `gateway_failure=durable_gateway:checkpoint_load_failed` (or that
   class as the container-log fatal). Run `37166456262` timed out with
   `active:1,healthy:0` (starting but never healthy — not the
   `active:0,healthy:1` crashloop signature) and run `37166919319` failed
   earlier with `api_transport_failure`. Available log tails name no
   `checkpoint_load_failed`. Limit: container stdout is dashboard-only, so
   the log half is not observable from an agent seat; the verify half is
   fully evidenced. Replay steps are in §5.
2. **T2.** Both halves evaluated: no freeze declared (first half has nothing
   to fire on), and exactly one `staging-migrate` run exists in history —
   the pre-gate refused one — so no `expected_pending` mismatch refusal
   occurred (second half has nothing to fire on). Standing caution, not a
   firing trigger: the refused run logged 32 expected-pending versions and
   staging holds zero `voice_*` tables, so a freeze declared today would be
   NO-GO until the governed migrate-then-redeploy path completes. Live
   loud-failure confirmation (next real dispatch must go red on a refusal)
   stays with the Operator-governed dispatch path.
3. **T3.** Structural coverage exists for jobs (`job_consecutive_failures`,
   `job_stale`), REST (`rest_429_rate`), pool and DB errors; voice-series
   coverage does not (six rules in `wrangler/src/alert-rules.ts`, none reads
   `two_bot_voice_*`). Two gap cards filed: the voice-failure rule (Worker
   slice with runbook anchor and shared packet spelling) and the live
   create/move/delete rehearsal on healthy staging (which also closes the
   ghost, gateway-gap, REST and job-staleness rows).
4. **T4.** `voice_rooms` (via `GREATEST(created_at, owner_touched_at, name_touched_at, privacy_touched_at)`) and
   `voice_room_blocks` and `voice_text_companions` are measured; `voice_creators`, the ten V11b
   config tables, `voice_logging_settings` and `voice_access_controls` are
   `Unmeasurable` with source reasons but no reviewed acceptance. Test-seed
   coverage for the measured tables is already in review elsewhere; the gap
   card filed here is the acceptance disposition (accept each reason or name
   the alternate evidence), without which any future window containing only
   voice-config writes keeps T4 open.
5. **T5.** The trigger compares freeze-time live definitions/permissions
   against the reconciled target. The T0 command-surface fixture
   (`command_surface`, 27 gates) gives the bootsurface half; the missing
   piece is the freeze read-back receipt (what is snapshotted, what target
   it is compared against, where the pair is recorded, who dispositions
   drift). Gap card filed for that receipt definition.

## 4. Gaps filed (one card each, on the drill card)

1. Voice-failure metrics alert rule (Worker slice, with runbook anchor and
   packet spelling) — from T3.
2. Live create/move/delete rehearsal on healthy staging, with ghost-count
   poll to baseline and gateway/REST/job receipt capture — from T3.
3. Reviewed disposition for the unmeasurable rollback-delta voice tables —
   from T4.
4. T5 freeze read-back receipt for command/permission drift — from T5.

Deliberately not filed: a freeze-time `pending_before` receipt (cutover
procedure already requires it; no window exists), a migrate-dispatch card
(Operator-governed path already owns it), ballot execution (existing
staging smoke owns it).

## 5. Reviewer replay (T1, ~10 min, public metadata only)

1. `gh run list --workflow=deploy-staging.yml --limit 5` → runs
   `37166456262` and `37166919319`, both `failure` at `Verify exact new
   rollout and serving Worker/container readiness`.
2. Read each failed job's tail (failed-jobs summary): `37166456262` ends
   `staging rollout gate failed: rollout_timeout` with last observation
   `rollout=completed instances=active:1,healthy:0,failed:0,starting:0,
   scheduling:0`; `37166919319` ends `api_transport_failure`. Neither names
   `checkpoint_load_failed`.
3. Open `docs/voice-cutover-rollback-triggers.md` §T1: firing needs
   `rollout_timeout` **and** the `checkpoint_load_failed` naming. Condition
   not met → NO-FIRE. Expected replay result: agree, or cite the line that
   changes the verdict.

## 6. Limits

- Offline only: no `/readyz`, `/ops/metrics`, container-log or database
  contact from this drill; staging halves rest on public CI metadata and
  merged source.
- Container-stdout halves (T1 log class, `voice_event` emission) are
  dashboard-only by established verification notes; absence here is "not
  observed from this seat", never proof of absence on staging.
- T0 fixture pins `origin/main` at authoring revision `27d22b42`; drift
  since is not re-verified here.
