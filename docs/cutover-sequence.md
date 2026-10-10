# Forward cutover sequence: freeze, migrate, deploy, verify, watch handoff

Sequenced T0 run-sheet for the production cutover. It answers *in what
order the forward steps run* and what each step records. It is a
**procedure, not approval to execute it**.

- Full safety contract, data-copy semantics and abort triggers live in
  [cutover.md](cutover.md).
- Backout lives there too: this sheet never duplicates it. On any failed
  gate, follow [cutover.md §Rollback](cutover.md#rollback-preserve-next-window-writes-before-reopening-legacy).
- Single-gateway ownership (Coolify warm-but-stopped, one session at a
  time, stop/start order) lives in the companion
  [guard checklist](cutover-guard-checklist.md); go/no-go evidence lives in
  the [pre-flight sheet](cutover-preflight-checklist.md). This sheet links
  to both instead of repeating them.
- Deploy mechanics and the watch log live in
  [production-deploy.md](production-deploy.md); operator observations live in
  the [runbook](runbook.md).

How to use: the cutover lead walks phases 0–5 top to bottom on the
execution card. Each step names its command, the evidence to record, and
the rollback pointer to follow on failure. Any failed gate is NO-GO:
freeze, keep maintenance closed, and follow the pointer. Host steps run
only through the authorized broker or an `Operator:` handoff with command
and rollback — never from an agent shell. Keep member data, dumps and logs
in the restricted evidence location, never in public comments.

## 0. Pre-freeze checks

Complete before `T_f`. No gateway is stopped yet.

| # | Step | Command | Evidence | Rollback pointer |
|---|---|---|---|---|
| 0.1 | Candidate pinned | Read the merged head: `git rev-parse HEAD` on a clean `main` checkout; confirm the latest `check.yml` run/current attempt completed successfully with its full `ci-ok` verdict (lint and all selected Rust/DB lanes) and `worker check`, plus green `pr-lint` and `gitleaks` on that exact head. A lint-only `check` is insufficient | Full 40-hex SHA, CI run/attempt link, independent review link, digest pinned on the execution card | [cutover.md §Preconditions](cutover.md#preconditions-all-must-pass); [pre-flight sheet §1](cutover-preflight-checklist.md#1-candidate-pinned-and-ci-green-on-the-exact-head) |
| 0.2 | Staging receipt for that SHA | Confirm the successful `deploy-staging` run for the same SHA; probe `curl --include "${STAGING_WORKER_URL}/health"` and `/readyz`, compare compiled `build_revision`/`build_id` to the SHA | Staging run link, `/health` 200, `/readyz` status plus revision match | [production-deploy.md](production-deploy.md) staging gate; [pre-flight sheet §2](cutover-preflight-checklist.md#2-staging-deploy-healthy-for-that-sha) |
| 0.3 | Announce and staff (T−24 h) | Attach the evidence manifest and sign-offs to the execution card; announce the window to moderators | Manifest link, window times, manual-moderation coverage named | [cutover.md §T-minus](cutover.md#t-minus-checklist) |
| 0.4 | Record pending work (T−60 min) | Through the authorized data/moderation leads: list outstanding tempbans/unbans, timeouts, tickets, voice rooms, feeds, scheduled messages, pending web/internal actions and retry leases | Absolute expiry times, IDs and claimed states; role/intent/policy read-back | [cutover.md §T-minus](cutover.md#t-minus-checklist) |
| 0.5 | Read-only preflight (T−15 min) | Authorized REST executor only, no gateway startup: token/application identity, intent flags, role/channel results and session-start budget; snapshot legacy command definitions **and** separate guild permissions before any overwrite | Preflight FAIL/WARN disposition; frozen definition + permission snapshots | [cutover.md §T-minus](cutover.md#t-minus-checklist); permission capture in [cutover.md §Command definitions](cutover.md#command-definitions-and-separate-guild-permission-recovery) |

If 0.1–0.5 is not all green, do not declare `T_f`. Rollback pointer for
this phase is trivial: nothing stopped, nothing copied — record the NO-GO
and reschedule.

## 1. Freeze and drain (`T_f`)

| # | Step | Command | Evidence | Rollback pointer |
|---|---|---|---|---|
| 1.1 | Freeze writers | Via the reviewed maintenance mechanism: freeze new bot commands and web/internal-action writes; pause every producer/cron/consumer that can mutate the copied data | Frozen-writer list, maintenance receipt | [cutover.md §Freeze](cutover.md#freeze-and-drain-t_f); ownership order in the guard checklist |
| 1.2 | Drain | Let admitted handlers finish; observe queue depth, in-flight requests, due unbans, last good jobs and delivery/replay ledgers until zero admitted work (or explicitly preserved items) | Queue-depth readings, preserved-item list | Same as 1.1: a process stop alone is not drainage proof |
| 1.3 | Stop legacy | Graceful stop via Coolify/broker; confirm auto-deploy, restart supervision and duplicate replicas cannot reconnect it | Legacy last event, terminal state, `T_f` timestamp, final writer watermarks | Guard checklist §1–§3; if any writer cannot be fenced, **abort before copy** per [cutover.md §Freeze](cutover.md#freeze-and-drain-t_f) |

## 2. Migrate, in order

Apply through the authorized operator path with the migrator login
(`SET ROLE two_bot_migrator`); the gateway runs DML-only and never
migrates at startup. URLs come from approved secret bindings, never
arguments or logs.

| # | Step | Command | Evidence | Rollback pointer |
|---|---|---|---|---|
| 2.1 | Roles baseline | Operator renders `two-bot db roles plan --phase bootstrap` (first bootstrap) or `two-bot db roles plan` (later runs); operator applies the rendered SQL; then `two-bot db roles verify` | Rendered plan ref, verify exit 0, pre-change ownership/ACL inventory saved | [database-roles.md](database-roles.md) operator boundary and deployment order |
| 2.2 | Plan the pending set | Migration runner `--plan` (read-only): prints the pending list — every source version absent from the ledger, in source order — plus per-migration hashes in the manifest | `staging-migrate-manifest.json`-style manifest (source SHA, pending list, hashes), ledger-before snapshot | [runbook §Staging-only migration runner](runbook.md#staging-only-migration-runner); never invent flags |
| 2.3 | Apply the pending set | Runner `--apply` bound to the reviewed plan (requires the exact `--expected-pending` list plus the plan manifest hash and run id; refuses on any mismatch before DDL) | Ledger-after snapshot, applied count, schema versions and hashes | Same runner section; a hash mismatch refuses — reconcile, do not force |
| 2.4 | Allocator gate | Reviewed migration reconciles **every** imported generated-key table allocator (sequence/identity state, high-water marks including deleted IDs); rehearse default-ID inserts on disposable fixtures | Per-table receipt proving the next allocation cannot collide or reuse reserved IDs | [cutover.md §Data copy](cutover.md#data-copy-and-verification); missing receipt is NO-GO |
| 2.5 | Content verification | Per-table counts **and** canonical per-key/content checksums, key preservation, timestamps/time zones, JSON settings, tombstones/deletions and pending/completed ledger states; zero unexplained mismatches; record the baseline watermark | Counts/hashes/conflict record, baseline watermark | [cutover.md §Data copy](cutover.md#data-copy-and-verification) |
| 2.6 | Voice configuration (V11) | Operator applies the reviewed V11 document with `voice-config-apply`: dry run `voice-config-apply --guild <id> --file <v11.json>`, then `--apply --expect-hash <hash>`; invocation and exit codes in [voice-config-apply.md](voice-config-apply.md) | Diff plus hash receipt, applied read-back | [voice-config-apply.md](voice-config-apply.md); on any failed gate follow the [rollback path](cutover.md#rollback-preserve-next-window-writes-before-reopening-legacy) |

Migration groups run in source order: funnel/levels first, then feature
migrations in numeric order (`0110` moderation through `0390` legacy-copy
presence, `0320`–`0321` gateway sessions/directives, `0330`–`0340` settings
and audit, `0400`–`0407` store foundation where applicable), then the web
contract. The pending-set computation in 2.2 defines the exact list for the
candidate; do not hand-pick a subset. Start neither gateway while 2.5 is
unresolved.

## 3. Deploy

| # | Step | Command | Evidence | Rollback pointer |
|---|---|---|---|---|
| 3.1 | Registry reconcile | Authorized REST executor: `commands diff` against the **live** snapshot; approve the reconciled target (reviewed Next registry with preserved access controls); apply overwrites; map returned IDs; reapply reconciled per-command permission overrides via the authorized Bearer executor; read back every affected guild | Definition + permission snapshots, approved target, old/current/restored ID maps, read-back with zero unexplained mismatches | [cutover.md §Command definitions](cutover.md#command-definitions-and-separate-guild-permission-recovery) and [§Registry swap](cutover.md#registry-swap-first-boot-and-gono-go) |
| 3.2 | Stage secrets and config | Bind the existing application token and approved DB/signing secrets through the secret service while the gateway is stopped; keep the Worker/DO fence active | Binding names only (never values), staged-config receipt | [cutover.md §Registry swap](cutover.md#registry-swap-first-boot-and-gono-go) |
| 3.3 | Arm first-boot IDENTIFY | From an operator checkout: `cargo run -p two-bot-cutover --bin gateway-force-identify --locked -- --guild "$GUILD_ID"` (dry run), then with `--apply --reason "first production boot" --allow-live-guild` | Directive armed receipt; follow-up dry run later shows `consumed at` | [runbook §Force-fresh IDENTIFY](runbook.md#force-fresh-identify-first-production-boot-only); never import legacy session state |
| 3.4 | Dispatch production | Dispatch `.github/workflows/deploy-production.yml` from `main` with `sha` set to the pinned 40-hex commit (rollback input empty) and `takeover` set to `true`. The job checks out exactly that commit, re-verifies ancestry, renders the build-identity config (`BOT_BUILD_REVISION` is that SHA, `BOT_BUILD_ID` is the run id and attempt), runs `wrangler deploy --config <rendered> --env production --message <sha>`, records the SHA plus old/new Worker version IDs, then runs the P2/P3 takeover in step 3.5 below and gates `/readyz` on that revision and this run's build ID. A dispatch with `takeover` left `false` is a routine deploy: the ownership fence stays held, fenced `/health` and `/readyz` answers carry no build fields, and the gate run is expected to fail (not a rollback signal). If `PRODUCTION_AUTO_APPROVE` is `true`, the summary says so; the variable replaces the reviewer only when the Environment has none, and a repository administrator enables it only on a CEO and CISO decision ([production-deploy.md](production-deploy.md#production_auto_approve)) | Dispatch link, run summary (SHA, old + new version IDs, approval mode, takeover requested), guard pass, `/readyz` revision match | [production-deploy.md](production-deploy.md) build identity, `/readyz` gate, [takeover step](production-deploy.md#production-ownership-takeover-p2p4) and environment gates |
| 3.5 | Release and start one | After the lead confirms every data/registry/permission gate, the same 3.4 dispatch runs the implemented takeover ([production-deploy.md §Production ownership takeover](production-deploy.md#production-ownership-takeover-p2p4), design [cutover-production-takeover-design.md](cutover-production-takeover-design.md) P2–P4): P2 `Read production ownership state without starting` pins the P1 new Worker version ID and refuses on any mismatch, P3 `Take over production ownership at the read epoch` posts exactly the P2 epoch with the explicit release and a run-derived audit actor, and the gate plus the post-takeover version re-read verify **one** Next container. Then read `/readyz` and compare the reported revision and build ID against the 3.4 run summary | Takeover receipt (actor, epoch, `phase=active`, `running=false`), first READY, `/readyz` revision/build-ID match, post-takeover version still at 100%, remaining session-start budget | Guard checklist §2–§3; [cutover.md §Registry swap](cutover.md#registry-swap-first-boot-and-gono-go); on any failed gate follow the [rollback path](cutover.md#rollback-preserve-next-window-writes-before-reopening-legacy) |

## 4. Verify gates

| # | Step | Command | Evidence | Rollback pointer |
|---|---|---|---|---|
| 4.1 | Readiness | `curl --include "$PRODUCTION_WORKER_URL/health"` (200) and `/readyz` (200 with all components ready, exact compiled revision/build ID); re-read rollout/Worker control-plane state after the probes | Statuses, revision match, rollout ID | [production-deploy.md](production-deploy.md) `/readyz` gate; 503 parked is truthful, never acceptance |
| 4.2 | Event continuity | Compare real join/message/voice observations against the moderator record; verify agreed non-destructive command/web journeys; confirm due jobs/unbans reconciled before releasing their single consumer | Moderator cross-check, journey receipts, job last-success times | [cutover.md §Registry swap](cutover.md#registry-swap-first-boot-and-gono-go) GO/abort lists |
| 4.3 | GO decision | Lead declares GO only with: no overlapping gateway/writers; final data/registry checks match; preflight no FAIL; fresh READY with all required components healthy; no unexplained gap, duplicate or missed deadline; all sign-offs and rollback receipts present | GO record with `T_0` (first `/readyz` 200 on the production revision) | Any abort trigger starts the [rollback path](cutover.md#rollback-preserve-next-window-writes-before-reopening-legacy) through the single production rollback dispatch with `takeover: true` ([production-deploy.md](production-deploy.md); coverage: dry-walk [runbook §7](cutover-rollback-runbook.md#7-staging-rehearsal-log), no executed dispatch), not a retry loop |

Release producers in the recorded order only after GO. `RESUME` may then
be enabled only with Next's own persisted session, by the reviewed
configuration path.

## 5. Watch handoff

`T_0` is the first `/readyz` 200 on the production revision. The lead
records each checkpoint with `scripts/cutover_watch_checkpoint.py` — one
checkpoint per call — and pastes the emitted row onto the execution card,
from `T_0` through `T_0 + 48 h`:

```sh
python3 scripts/cutover_watch_checkpoint.py --checkpoint +15m \
    --expected-sha <40-hex> --expected-build-id <run-id>-<attempt> \
    --production-url https://<production-worker>/
```

`--checkpoint` is one of the five labels `+15m`, `+1h`, `+6h`, `+24h`,
`+48h`; `--expected-sha` is the deployed commit from the watch header and
`--expected-build-id` is that deploy run's `<run id>-<attempt>` from the
run summary. The script is read-only: one GET to `/readyz`, no writes,
migrates, or DB connections. It emits GO only on a 200 with every
component ready and an exact revision/build-ID match; anything short of a
full match is EXTEND, never GO, and a ROLLBACK decision stays human.

- Header: `T_0`, deployed SHA, new and previous Worker version IDs (the
  previous ID is the rollback dispatch `<version-id>`), watch deadline,
  named coverage.
- Cadence: short read-only polls (suggested 60 s) plus deployment events
  and moderator observations; checkpoints at +15 min, +1 h, +6 h, +24 h,
  +48 h. Record findings, not every healthy poll.
- Signals and budgets: `readyz`/revision, gateway session starts, REST
  429/5xx and latency, unban queue, scheduled jobs, DB pool, RSS/placement,
  event continuity, shutdown drain — thresholds in
  [production-deploy.md](production-deploy.md#signal-thresholds-budgets-and-rollback-triggers).
  Any unexplained gap or duplicated execution freezes writers and evaluates
  rollback.
- Handoff criteria: named coverage plus a **real configured** monitor are
  recorded before handoff; at +48 h record sign-off or extend the watch on
  the execution card. Legacy stays warm until separate retirement
  authorization.

## Communication

Announce state changes with the
[template](cutover.md#communication-template) (private detail versus
public notice split): state, UTC/lead, window, gateway owner, evidence
links without secret values, writes/watermark, decision and loss, gap,
next update and next actor/action.
