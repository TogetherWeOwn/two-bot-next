# Bot production-cutover rollback runbook

Operator checklist for rolling the production cutover back to the legacy
gateway. This is a **procedure, not approval to execute it**. Authority
lives in the linked docs; this page only orders the decision into one
checklist. Keep member data, dumps, payloads and logs in the restricted
evidence location, never in public GitHub or issue comments.

- Full cutover procedure, ownership fence and data/registry recovery:
  [cutover.md](cutover.md).
- Production deploy/rollback dispatch, watch log and signal budgets:
  [production-deploy.md](production-deploy.md).
- Staging redeploy, Worker-version rollback and containment boundaries:
  [runbook.md](runbook.md) (`Redeploy and rollback`, `Containment`).
- Staging rollout gate and provenance chain:
  [staging-rollout-gate.md](staging-rollout-gate.md).
- Next-window delta sizing (read-only): [rollback-delta.md](rollback-delta.md).

## 1. Decider

- The **cutover lead** owns go/no-go, timing, the 48-hour watch and the
  rollback declaration, including the recorded reason and loss statement.
- The **director of engineering** resolves technical disputes (for example
  an incomplete journal or an unreconciled registry map). The lead cannot
  waive the zero-loss gate below.
- The production dispatch additionally needs a **production environment
  reviewer** approval on the exact guarded commit, unless the repository
  variable `PRODUCTION_AUTO_APPROVE` is `true` and the Environment has no
  reviewers ([production-deploy.md](production-deploy.md#production_auto_approve)).
  The executor performs only separately authorized steps, one host step per
  operator handoff.

## 2. Trigger conditions

Any row fires the same first action: **freeze all Next and web writers,
keep the current gateway fenced, investigate**. Roll back when no
recovery path is identified by the checkpoint. Thresholds are budgets in
[production-deploy.md](production-deploy.md); the stop conditions below
come from [cutover.md](cutover.md) §§Registry swap and Rollback.

| Signal | Rollback-trigger value |
|---|---|
| Readiness | `/readyz` 503 sustained past 60 s after a restart or deploy event, with no identified recovery path |
| Crash loop | Consecutive starts never reaching 200, or repeated supervisor restarts |
| Session budget | Restart loop consuming the recovery reserve: repeated fresh IDENTIFYs, invalid-session storm, or a budget reading that no longer allows recovery |
| REST | Breaker open, or the 429-rate alert firing across consecutive samples after containment, on a route this revision introduced |
| Jobs | A watch-critical job stale (last success older than two cadences) or 3 consecutive failures, caused by this revision |
| Data/journal | Durable capture gap over any acknowledged write or effect; any uncertain send without a recorded disposition |
| Events | Any unexplained gateway-event gap or duplicated execution versus independent moderator observation |
| Moderation | Any overdue sanction without a named disposition that the restored single consumer cannot reconcile |
| Registry | Auth, intent or permission failure; unmapped registry or ambiguous command-ID map; any unexplained permission mismatch |
| Resources | Sustained RSS growth versus the soak floor pressing the placement cap, or any OOM-kill |

## 3. Time bounds

- **Drain:** reach zero admitted work (queue depth, in-flight requests,
  claims, replay ledgers) before stopping either gateway. A process stop
  alone is not proof of drainage.
- **First-ready gap:** first `/readyz` 200 within 60 s of a restart or
  deploy event; sustained 503 past that freezes writers.
- **Watch:** 48 h from first production ready (`T_0`), with checkpoints
  at +15 min, +1 h, +6 h, +24 h and +48 h. Extend the watch on
  uncertainty; never close it on a failed watch.
- **Shutdown drain:** 35 s per SIGTERM; a second signal exits immediately.
- **Session checkpoint:** at most 15 min old for RESUME; first production
  boot forces a fresh IDENTIFY through the reviewed one-shot path.
- **Rollback completion:** bounded by the rehearsed drill measurement; a
  missed budget is an incident, not permission to skip reconciliation.
- The ownership fence stays active throughout legacy ownership, across
  deployments and the retirement wait, until the lead hands Next back.

## Production decision window

A recorded host decision established a 2-hour rollback decision window from
the cutover start time (`T_0`). Writes acknowledged inside that window are
accepted as lost if a rollback occurs. After the window closed, the recovery
posture is forward-fix. Reopening legacy remains subject to the zero-loss
reconciliation gate in §6 plus separate reopening authority; the decision
window does not relax that gate.

## 4. Ordered rollback steps

1. Declare rollback with UTC time and reason. Freeze all Next and web
   writers and producers; do not start legacy yet. Activate and verify
   the persisted Worker ownership fence **before** stopping Next, pause
   health callers and fence keepalive, admitted fetches and auto-start
   paths. Drain admitted work, stop Next, verify terminal state from
   control-plane and log receipts. Record rollback freeze `T_r` and final
   durable watermarks; preserve a restricted snapshot of Next data.
2. Fence registry and permission writers (including administrator edits)
   and capture final live command definitions, guild permissions and
   defaults/inheritance **before any restoration**. Keep those writers
   fenced through read-back. If either capture fails, keep maintenance
   closed and do not start legacy.
3. Reconcile **every write since the freeze baseline**, including config
   updates and deletes, XP and onboarding state, tickets and transcripts,
   schedules and feeds, moderation actions and unbans, voice ownership,
   audit and internal-action ledgers. Use the tested per-table and
   per-key mapping over a transactionally consistent capture, not a bare
   timestamp filter (which misses deletes and ledger rows). Record
   counts, hashes, conflicts and the final applied watermark.
4. Classify applied Discord effects from delivery, audit and replay
   receipts. Completed messages, sanctions, role changes, unbans and
   callbacks are **not** replayed or undone by a database restore.
   Reconcile uncertain effects explicitly with moderators; transfer
   leases only after old owners are fenced.
5. Reconcile the frozen final live registry snapshot and approved
   watch-window edits against the pre-swap and post-swap receipts, and
   approve a **reconciled target** (never an automatic reset to the old
   baseline): preserve legitimate additions, deletions, renames and
   revocations, including revoked role allows and changed inherited
   defaults. Apply definitions first, retain actual returned IDs with
   old/current/restored maps, reapply complete reconciled override sets
   through the authorized permission path, then read back every affected
   guild against the target. Any unexplained mismatch keeps commands
   frozen with a named disposition.
6. Restore the pinned legacy image and configuration with the reconciled
   database binding, keeping the existing application token. Pass
   read-only legacy preflight, confirm the Next fence is still active
   and Next is stopped, and start **one** legacy gateway with its tested
   fresh-session procedure, never with Next session state. Record first
   READY and verify health, real event continuity, internal actions and
   pending jobs before reopening writes.
7. Resume producers and consumers once, in the recorded order. Compare
   post-rollback watermarks and the command registry, watch the
   recovered service for at least the measured drill recovery window,
   announce restored ownership and record incident, loss, gap and
   reconciliation evidence. Keep Next evidence intact and its fence
   active; recovered monitors target legacy, never an auto-starting Next
   health route.
8. If journal capture is incomplete or reverse reconciliation fails, keep
   affected writes in maintenance, preserve both data sets and escalate
   a decision brief. Never silently accept data loss to restore
   availability.

## 5. Cloudflare revert (no DNS change)

- This cutover changes **no DNS records**. The bot keeps the same
  Discord application; traffic moves between Cloudflare Worker versions
  and container images. The checked-in Worker configuration declares no
  custom routes or domains; the invite-redirect snapshot is Worker
  configuration, not DNS.
- The production workflow dispatch below is the single production
  rollback method. Revert by dispatching the production workflow with
  `takeover: true` and the previous Worker version ID recorded in the
  watch header. The guard requires a full commit on the main branch
  with green checks and a successful staging run, plus reviewer
  approval; the workflow fails unless the target version serves 100%
  of traffic, then re-runs the `/health` 200 and the build-identity
  `/readyz` gate. The gate proves the rolled-back revision serves only
  after the takeover order releases the fence: fenced answers carry no
  build fields and fail the gate by design
  ([production-deploy.md](production-deploy.md#build-identity-and-the-readyz-gate)).
  Coverage: §7 below is a dry-walk that checked this route without
  executing a rollback or deploy; the staging rollback drill
  ([ci-security.md](ci-security.md#staging-rollback-drill-manual),
  [runbook.md](runbook.md#worker-version-rollback)) rehearses fence,
  unforced deployment with immediate Durable Object update, takeover
  and restore, which differs from production's `rollback --yes`
  (auto-confirms the changed-secrets prompt) with deferred Durable
  Object default.
- A Worker-version rollback does **not** rebuild the container image or
  rewind data. A standalone full redeploy of a known-good pair is
  **superseded as a production rollback path**: when the Rust image is
  the fault, dispatch the same production workflow in deploy mode with
  `takeover: true` and the prior good SHA (guard, takeover order and
  `/readyz` build-identity gate apply unchanged), only after confirming
  it supports the current schema and bindings, then confirm the running
  image separately. That deploy-mode path has no production drill
  record; the closest analogue is the digest-pinned full-rollout
  staging container drill in
  [runbook.md](runbook.md#worker-version-rollback), which is not a
  production dispatch.
- Never roll back to a pre-fence wrapper version: it ignores the
  persisted ownership record and can restart an unauthorized gateway.
  Keep a reviewed fence-capable known-good pair recorded before rollout.

## 6. Data-compat notes

- **Shared canonical database:** legacy returns to the same current
  data. Additive Next schema is retained; never down-migrate, restore
  an old snapshot over it, or restart legacy against a divergent copy.
  Validate legacy compatibility against the retained schema first.
- **Separate legacy database:** reconcile into a recovered compatible
  target from the freeze baseline plus the full Next **and** web delta,
  preserving primary keys, tombstones, allocation state, deadlines and
  completion or claim state, including Next-created records with no
  legacy counterpart. An unsupported mapping blocks reopening; it never
  permits dropping those records.
- **Maximum accepted loss: zero acknowledged committed writes** over the
  whole Next window. Restoring to the freeze baseline alone loses that
  window and is not an acceptable rollback.
- **Allocator gate:** reconcile every imported generated-key allocator,
  including high-water marks, deleted IDs and sequence semantics, and
  prove the next allocation cannot collide before releasing any writer.
  An archive without allocator marks does not meet this gate.
- The delta report is read-only sizing inside one repeatable-read-only
  transaction; it never copies rows. A backup restore truncates and
  replaces allowlisted tables on a pre-migrated target; it is not a
  merge. Discord effects already applied are not undone by any restore.

## 7. Staging rehearsal log

| Field | Value |
|---|---|
| Date (UTC) | 2026-10-03 |
| Revision tested | `87d980608c4f0ea36daf2ff40920e28ee0ee8d6b` (short `87d9806`) |
| Scope | Read-only dry-walk of this runbook against staging configuration. No rollback, deploy, DNS change or database write was executed. |
| Staging worker path checked | Production-workflow rollback route (`rollback <version-id>` with 100%-traffic assertion and `/health` + truthful-`/readyz` gate) and the pinned Wrangler 4.147.0 aliases; Worker configuration declares no custom routes or domains, so the revert is version plus image, not DNS. |
| Staging guild path checked | Staging guild pin matches between the soak record and the `staging_guild_id` gate in `crates/core/src/backup/guild_config.rs`; guild-config capture and restore refuse without that pin and the staging confirmation flag. |
| Delta sizing path checked | `rollback-delta` contract: single `REPEATABLE READ, READ ONLY` snapshot transaction over the classified table matrix, with an export cap that refuses rather than truncates. |
| Live probe | Not executed from this workspace: no live staging credential is provisioned here, and a live rollback or deploy would move staging traffic. Live `/health` and `/readyz` observation plus the redirect smoke stay with the authorized staging executor. |
| Verdict | **PASS** (dry-walk scope). The ordered steps, revert route, compat notes, decider and bounds above match the shipped workflow, Worker configuration and tool contracts at the tested revision. |

Future rehearsals append rows with the same fields; a row whose live
probe or dispatch was actually executed names its receipts.
