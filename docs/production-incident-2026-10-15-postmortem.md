# Postmortem — 2026-10-15 production gateway crash-loop

**Status: root cause PROVISIONAL — non-reproduction recorded 2026-10-17.**
The cause below is the leading hypothesis from the incident investigation
(production 19 migrations behind staging). The RCA fix PR (#978:
`failed_step` + error-code logging, schema-gap survival) merged on
2026-10-15 at 21:35Z. As of this revision no production `gateway_failed`
log line from a #978 build naming the failing step is quoted anywhere in
this repo: the 10-15 logs carry no cause chain, and the only post-#978
production crash-loop on record (2026-10-16 role drift, SQLSTATE `42501`,
see the role gate in [production-deploy.md](production-deploy.md)) quotes
no `failed_step` line either — and its mechanism (missing grant, cursor
held on rejoin) is distinct from this hypothesis (missing table `42P01`).
The retention write cannot confirm this hypothesis via `gateway_failed` on a
#978+ build: `retention_lifecycle` classifies `42P01` as a schema gap
(`crates/bot/src/gateway.rs:655`, `is_schema_gap` at
`crates/bot/src/gateway_failure.rs:198`), logs `retention lifecycle
skipped; schema gap, nothing to cancel` with `error_code=42P01`, and returns
`None` — so that path emits no `gateway_run` step failure
(`crates/bot/src/gateway.rs:877-887`, `crates/bot/src/main.rs:700`) and no
`gateway_failed` line (`publish_gateway_failure` at
`crates/bot/src/main.rs:861`). The observable #978-era signal for the
retention trigger is that schema-gap skip line with `error_code=42P01`. The
section's second trigger (XP awards inserting into `member_season_xp` via the
leveling drain) still discarded its SQLSTATE in the #978-era implementation:
`leveling_dispatch_failure()` synthesized `InvalidArgument`, and the outer
error arm replaced even a checkpoint deadline with the same literal. The
`gateway_run` failure therefore carried `error_code=none`.

**Dispatch-cause fix receipt (2026-10-23):** `apply_dispatch` now extracts the
underlying database error from both store and post-award role-store failures.
Only `42P01`/`42501` auxiliary drains skip, with the fixed warning
`gateway leveling dispatch skipped; schema gap`, `drain=leveling`, a bounded
`cause`, the SQLSTATE and `two_bot_gateway_drain_skips_total`. Non-gap errors
retain their source through `run_shard` into `StepFailure.error`;
`publish_gateway_failure` can report that SQLSTATE through the existing
`error_code` field. Deadline failures keep their own bounded reason. The three
non-database dispatch failures keep their distinct fixed literals and
`error_code=none` (dropped interaction completion, invalid onboarding job,
missing ingress ticket; only the last is post-commit). Protected regressions
in `crates/bot/src/gateway_tests/dispatch_cause.rs` fault-inject real `42P01`,
`42501` and `23514`, verify checkpoint advance/hold and sanitized logs, and keep
a healthy XP-write control. Retention behavior is unchanged. This is a code
and regression receipt, **not** a production observation or an RCA confirmation;
no production migration, restore or alert deployment is performed. A
`gateway_failed` confirmation of the retention trigger would need a pre-#978
build or a staging restore. Update this section when such a line is captured;
do not treat the hypothesis as proven before then.

## Timeline (all UTC, 2026-10-15)

| Time | Event (receipt) |
|---|---|
| 15:48:47Z | `deploy-production` run [38763610825](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/38763610825) dispatched in deploy mode for `7302cc3eded75085f71e53e8bb4d79d4e776d19a`; the `sha guard` refused it: `Refusing production deploy: deploy-staging has no successful run on 7302cc3eded75085f71e53e8bb4d79d4e776d19a`. Deploy and release jobs skipped. |
| 15:50:02Z | `deploy-production` run [38763788270](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/38763788270) dispatched for commit `6baab871d31c076f89b45db644e6bddef8d447cd`, with ownership takeover. |
| ~15:57Z | New container serving. It crash-looped about every 90 s: READY + GUILD_CREATE, then `durable gateway failed; checkpoint unchanged, readiness unavailable` (`gateway_runtime_failed`, `durable_gateway` phase), then shard close 1000 and shutdown. |
| 15:58:51Z | The deploy's `/readyz` gate polled once, read 503 with gateway `starting`, and **passed the deploy as healthy** (detection gap below). Job `wrangler deploy + readyz gate` completed success 15:58:57Z; run concluded success 15:59:03Z. |
| 16:00–16:12Z | Container logs show the repeated READY → `gateway_runtime_failed` → shutdown cycle. The same commit stayed green on staging (deploy + voice synthetic 14:18/14:20Z). |
| 16:05:30Z | Workflow rollback run [38766033550](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/38766033550) refused in the `sha guard`: `Refusing production rollback: sha must be a full 40-character hex commit SHA` (malformed `sha` input). Deploy and release jobs skipped. |
| 16:09:45Z | Workflow rollback dispatched as run [38766633962](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/38766633962): `sha` = `7302cc3eded75085f71e53e8bb4d79d4e776d19a`, `rollback` = `952618ab-771f-427d-b957-9037d6f7bb73`, `takeover: true`. The `sha guard` job **refused it in 14 s** (recovery gap below); deploy and release jobs skipped. |
| ~16:1xZ | Host rolled back directly via the Cloudflare API (outside the workflow): Worker version `952618ab` plus container image `sha256:bfe1038e` (built from `33e653d2c43e91a89842549d8fbdffcb13092b45`), takeover epoch 19. |
| 16:19Z | `/readyz` 200 on the rolled-back version. |
| 16:20Z | A lobby move succeeded. Production declared recovered. |
| 17:41Z | Read-only host schema receipt: production bot database 73 applied / max version 423; staging 92 applied / max version 441 — **production 19 migrations behind staging** (catalog/ledger only; no customer data read, nothing written). |

## Root cause (provisional)

Production lacked 19 migrations that staging had: `0100` (participation
rewards antigaming), `0101` (seasonal decay watermark), and `0424`–`0433`
plus `0435`–`0441` (internal-action outcome/audit, community facts, event-store
capture slices, privacy opt-out, backfill cursors, member catch-up, event-store
S1 core, retention queues, stream live claims, game activity aggregates).
There is no `0434` in `crates/cutover/migrations` (counted there), so the
gap is exactly these 19. (`crates/store/migrations/0434_privacy_opt_out.sql`
exists outside the cutover set, as does `0442_game_activity_guild_aggregates.sql`.)

Fitted mechanism (code-verified at `6baab871`; log proof pending a #978 build): on the first
`GUILD_CREATE` after boot, `retention_lifecycle`
(`crates/bot/src/gateway.rs:611`, inner call at `:626`) calls
`cancel_guild_purge` (`crates/cutover/src/retention.rs:527`, DELETE at
`:529`), which deletes from `retention_guild_purge_queue`
— created by migration `0438`,
missing in production. The build predates the schema-gap skip, so the
missing-table error holds the dispatch cursor, the worker stops, and the
gateway task returns `Err` → `gateway_runtime_failed`. The next boot replays
the same dispatch: a crash loop. Staging has the table, so it stayed green.
A second trigger sits behind the first: any XP award inserts into
`member_season_xp` (`crates/core/src/leveling_store.rs:275`, created by
`0100`, missing in production) and would hold the cursor the same way.

What is still missing: the 10-15 log line carries no cause/source chain, so
no log names the failing step. That is exactly what #978 adds (every
`*_failed` event carries `failed_step` plus the sanitized reason and SQL
error code; the retention lifecycle skips `42P01`/`42501` instead of holding
the cursor, except `42501` on an available `GUILD_CREATE`, which holds the
cursor). Until a log line confirms the step, the mechanism above is a
strong fit, not proof.

## Detection gap: the gate passed a 503 "parked" gateway

The deploy run's own log (job `wrangler deploy + readyz gate`, 15:58:51Z)
is the receipt. One poll, ~10 s after the new version took traffic:

```text
readyz -> 503
b'{"components":[["process","ready"],["gateway","starting"],["database","ready"],["token_invalid","ready"]],...,"build_revision":"6baab871d31c076f89b45db644e6bddef8d447cd","build_id":"38763788270-1"}'
gateway parked; deploy is healthy, revision matches the SHA, build 38763788270-1
```

The revision matched and the gateway was `starting`, so the
[build-identity gate](production-deploy.md#build-identity-and-the-readyz-gate)
recorded "gateway parked; deploy is healthy" and the run went green while the
container was already entering its crash loop. A 503 with gateway `starting`
is truthful as a state, but accepting it ~10 s after rollout — with no
follow-up ready observation and no committed-dispatch evidence — let a
crash-looping build pass. Sustained 503 past the recovery budget must freeze
writers and trigger rollback evaluation, never a pass; see the operator
section below.

## Recovery gap: the sanctioned rollback path refused

The 16:09Z workflow rollback (run 38766633962, inputs quoted in the timeline)
died in the `sha guard` with this exact refusal:

```text
##[error]Refusing production rollback: deploy-staging has no successful run on 7302cc3eded75085f71e53e8bb4d79d4e776d19a
```

Why: the guard requires a successful `deploy-staging` run on the exact rollback
SHA, and `7302cc3ed` was not the commit the working version was built from.
The only staging run on `7302cc3ed` (run
[38644156611](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/38644156611),
2026-10-14) concluded `failure`, so no green staging receipt existed — and
`7302cc3ed` was never known-good: it carries the same suspect
`retention_lifecycle` wiring (`crates/bot/src/gateway.rs:611` at both
commits, absent at `33e653d2`), so under this postmortem's own hypothesis a
build from it crash-loops on the behind production schema too. The version
that worked was built from `33e653d2` (tagged `v1.3.0`, green staging run
38316885340 and production deploy 38319357528 on 10-12). A rollback
dispatched with `sha` = `33e653d2` would have passed the staging check. The
16:09Z refusal therefore looks like a wrong-SHA input the guard correctly
caught — the dispatch did not follow the contract below (`sha` set to the
commit the previous version was built from) — not an admission-rule gap.
The 15:48Z deploy-mode refusal of the same SHA and the 16:05Z malformed-SHA
refusal (timeline) reinforce that the recovery story on 10-15 is input
derivation, not guard design. The host therefore rolled back outside the
workflow via the Cloudflare API. That path worked, but it bypassed the guard,
the environment approval, and the run-summary receipt chain — the drill
record for the deploy-mode prior-good-SHA path remains empty (see
[production-deploy.md](production-deploy.md): "The deploy-mode path with a
prior good SHA has no production drill record").

Pre-dispatch step: before any rollback dispatch, derive `sha` from the
target Worker version's recorded build SHA (the version Summary/metadata,
not the incident commit), and derive `image` from the pre-deploy rollback
snapshot in the failed run's summary. The current guard waives staging history
for rollback; do not re-dispatch with a guessed SHA or image.

## Rollback receipts (quoted, not re-executed)

Nothing here was re-run for this write-up. Values are quoted from the
10-15 incident record and the public run logs linked above.

Refused workflow rollback (run 38766633962, `sha guard` env, 16:09:51Z):

```text
SHA: 7302cc3eded75085f71e53e8bb4d79d4e776d19a
ROLLBACK: 952618ab-771f-427d-b957-9037d6f7bb73
TAKEOVER: true
```

That dispatch is the single sanctioned production rollback method
([production-deploy.md](production-deploy.md)): derive `sha` from the target
version's recorded build SHA first (pre-dispatch step above), then dispatch
`deploy-production` with `sha` set to the commit the previous version was
built from, `rollback` set to the previous Worker version ID from the failed
run's summary, `image` set to the previous container image digest from the
rollback snapshot, and `takeover: true`. The reviewed
`worker_container_rollback.py rollback` step restores both the Worker version
and container image, then runs the `/health` 200 + build-identity `/readyz`
stability gate; the workflow does not use `wrangler rollback`. When the Rust
image itself is the fault, dispatch in deploy mode with `takeover: true` and
the prior good SHA instead.

Successful host rollback (Cloudflare API, outside the workflow): Worker
version `952618ab` + container image `sha256:bfe1038e`
(`33e653d2c43e91a89842549d8fbdffcb13092b45`), takeover epoch 19;
`/readyz` 200 at 16:19Z, lobby move at 16:20Z. The literal host-side API
transcript lives outside this repo and is not quoted here — no command line
below invents it.

Pre-rollback version discovery (from [runbook.md](runbook.md#redeploy-and-rollback),
repository root, pinned Wrangler; staging shown, production needs its own gate):

```bash
npm --prefix wrangler run deployments -- list --env staging
npm --prefix wrangler run versions -- list --env staging
npm --prefix wrangler run versions -- view "${VERSION_ID}" --env staging
```

Record the current deployment and the known-good **Worker version + Git SHA
+ image** before changing anything; versions are not Git SHAs. Rollback does
not rewind Postgres or Durable Object storage — check whether migrations or
resource changes make the old code incompatible first.

## Operator runbook: crash-looping production gateway

### How to detect

1. Poll **both** routes; a 200 on `/health` alone proves nothing:
   ```bash
   curl --silent --show-error --max-time 10 --include "${WORKER_URL}/health"
   curl --silent --show-error --max-time 10 --include "${WORKER_URL}/readyz"
   ```
   (`WORKER_URL` is the affected environment's URL from the approved
   deployment record. Probes use an explicit agent string; the edge refuses
   the Python default agent.)
2. Crash-loop signature (10-15 pattern): container logs repeat READY +
   GUILD_CREATE → `durable gateway failed; checkpoint unchanged, readiness
   unavailable` (`gateway_runtime_failed`, `durable_gateway` phase) → shard
   close 1000 → shutdown, roughly every 90 s, while `/readyz` never reaches
   200 with gateway `ready`.
3. Distinguish `starting` from `parked-forever`: a gateway `starting` that
   outlasts the 60 s first-ready budget after a restart or deploy event is a
   finding, not health. Confirm against container logs, not the gate verdict.
4. If staging is green on the same SHA while production loops, suspect a
   production-only difference first: database schema/migration ledger,
   secret/config/flag, guild size or data shape, or checkpoint state the new
   build cannot read. Compare the migration ledgers read-only before any other
   theory.

### When to roll back

Roll back when **any** of these hold and no recovery path is identified by
the checkpoint (freeze all writers first; keep the gateway fenced):

- `/readyz` 503 sustained past 60 s after a restart or deploy event.
- Consecutive starts never reaching 200, or repeated supervisor restarts
  (crash loop).
- A restart loop consuming the session-start recovery reserve (repeated fresh
  IDENTIFYs, invalid-session storm).
- A watch-critical job stale past two cadences or 3 consecutive failures,
  caused by this revision.

Do **not** roll back to a pre-fence wrapper version (it ignores the persisted
ownership record and can restart an unauthorized gateway), and never silently
accept data loss to restore availability — reconcile per the
[cutover rollback runbook](cutover-rollback-runbook.md) ordered steps.

If the workflow guard refuses the rollback, stop and re-derive the inputs
before anything else: on 10-15 the record shows three refusals in sequence —
a deploy-mode refusal of `7302cc3ed` (15:48Z), a malformed-`sha` rollback
(16:05Z), then the wrong-SHA rollback (16:09Z). Do not retry with changed
inputs to slip past the guard. Derive `sha` from the target version's
recorded build SHA, confirm its staging receipt, then dispatch once.
Otherwise escalate to the host path with the recorded version ID, and file
each guard refusal as its own finding — refusals are evidence, not override
prompts.

### Who runs it

- The **cutover lead** owns go/no-go, timing, the watch, and the rollback
  declaration with reason and loss statement. The **director of engineering**
  resolves technical disputes; the lead cannot waive the loss gate.
- The production dispatch additionally needs a **production environment
  reviewer** approval on the exact guarded commit (unless automated approval
  is enabled and recorded). The executor performs only separately authorized
  steps, one host step per operator handoff.
- Production restores, token rotation, and live-guild changes need their
  separate authorization. An authentication/permission failure is a stop:
  report it; do not try another credential or elevate access.

## Follow-ups (not this doc's scope)

- Prod schema gate: DONE. `deploy-production` refuses a build whose
  migrations are ahead of the production ledger (schema gate, #979) and
  whose grants lag the migrations (role gate: `roles verify` must read 0,
  #1019). Rollback and live-SHA redeploy skip both gate jobs
  (guard-validated modes only; the production job accepts exactly those),
  so recovery never waits on them.
- Strict ready gate: DONE (#979). A 503-`starting` answer never passes a
  deploy on a single early poll: green requires 200 with gateway `ready`
  held for 18 consecutive polls (~3 min), and any parked answer keeps
  polling, then fails. After a takeover, a failed gate rolls back
  automatically when the deploy and pre-deploy snapshot succeeded;
  otherwise the run just fails without restoring. See the gate table in
  [production-deploy.md](production-deploy.md#build-identity-and-the-readyz-gate).
- Rollback inputs: record a pinned known-good Worker version + Git SHA +
  image after every production deploy (version discovery above), and make the
  rollback dispatch derive `sha` from that record. Do not widen the guard to
  admit any CI-green SHA: CI-green does not mean schema-compatible (a
  CI-green build carrying this defect would be admitted), and the 16:09Z
  refusal was a wrong-SHA input, not a rule gap.
- The fix itself and any live deploy or rollback are out of scope here.

## Sources

- Deploy run 38763788270 (breaking `6baab871` deploy + gate pass) and rollback
  run 38766633962 (guard refusal), linked in the timeline; job logs quoted above.
- Staging run 38644156611 (only staging run on `7302cc3ed`, concluded
  `failure`); deploy-mode refusal run 38763610825 (15:48Z) and
  malformed-`sha` rollback run 38766033550 (16:05Z), both refused in the
  `sha guard` with the strings quoted in the timeline.
- Working version provenance: `33e653d2` = tag `v1.3.0`, green staging run 38316885340 and production deploy 38319357528 (10-12).
- Host read-only schema receipt, 2026-10-15 17:41Z: production 73 applied / max 423 vs staging 92 / max 441.
- [production-deploy.md](production-deploy.md) (dispatch, guard, build-identity gate), [runbook.md](runbook.md#redeploy-and-rollback) (version discovery), [cutover-rollback-runbook.md](cutover-rollback-runbook.md) (ordered rollback steps), [gateway-recovery.md](gateway-recovery.md) (checkpoint contract).
