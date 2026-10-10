# Production takeover design (M1.5a)

Design for the production Worker/DO ownership takeover step (M1.5).
It answers *in what order the production singleton changes owner, what
refuses, what secrets it touches, and how it rolls back*. It is a
**design, not approval to execute it and not the execution sheet**.

- Forward cutover order, freeze/drain, data copy, registry swap, GO gates:
  [cutover-sequence.md](cutover-sequence.md).
- Full safety contract, data/registry recovery, fence protocol, rehearsal
  receipts: [cutover.md](cutover.md).
- Production deploy/rollback dispatch, watch log, signal budgets:
  [production-deploy.md](production-deploy.md).
- Ordered rollback checklist:
  [cutover-rollback-runbook.md](cutover-rollback-runbook.md).
- Staging-only gate (unchanged by this design):
  [staging-rollout-gate.md](staging-rollout-gate.md).
- Fence implementation:
  [`ownership.ts`](../wrangler/src/ownership.ts),
  [`index.ts`](../wrangler/src/index.ts);
  staging-only client:
  [`ownership-control.mjs`](../wrangler/scripts/ownership-control.mjs);
  production dispatch guard:
  [`.github/workflows/deploy-production.yml`](../.github/workflows/deploy-production.yml);
  Worker bindings:
  [`wrangler.toml`](../wrangler/wrangler.toml).

## Non-goals (explicit)

1. **Staging-takeover behavior unchanged.** The staging gate order
   (`preflight` -> `prepare` -> `wrangler deploy` -> `receipt` ->
   `deployment-takeover` -> `verify`), the staging-only client, and its
   origin pin stay as documented. Nothing here renames, repoints, or
   relaxes them.
2. **No production activation.** This design does not authorize releasing
   the fence, starting the production gateway, or declaring GO. Activation
   stays on the B4 execution card under the cutover lead with the CEO GO
   as the separate decision.
3. **No code.** This document introduces no new command, flag, workflow,
   binding, or secret. M1.5 (implementation) needs this design plus its
   own reviewed sheet.

## Background: what exists, what is missing

The persisted fence is implemented and shared by both environments: the
singleton `TWO_BOT.getByName("two-bot")` owns `two-bot:owner:v1`
(`deploymentId`, monotonically increasing `epoch`, `phase`,
`actor`, `timestamp`, `oldEpoch`, `oldDeploymentId`), where
`deploymentId` is the Cloudflare **Worker version ID** from
`CF_VERSION_METADATA.id` — never a Git SHA, label, or deployment
resource ID. Every startup entry (`start`, `startAndWaitForPorts`,
`containerFetch`), health/readyz, keepalive and `onStart` scheduling is
gated; the DO concurrency gate drains admitted starts/probes before an
ownership change; constructor reconciliation destroys an inactive
already-running process; stale-epoch keepalives drain without renewing.
Missing, malformed, or unreadable state fails closed. The fence is **per
singleton/namespace**: it cannot revoke legacy, another namespace, or a
separately started gateway.

What exists today is staging-only control: `ownership-control.mjs`
pins the `two-bot-next-staging.<subdomain>.workers.dev` origin, refuses
redirects, reads `OWNERSHIP_CONTROL_TOKEN` from an approved binding,
and **rejects production origins**. Production has the same fence code
but no reviewed production control path: B4's sheet must name one
before M1.5 can be carded. This design names it.

## Production takeover sequence

Runs inside the B4 window, after the data/registry gates that
[cutover-sequence.md](cutover-sequence.md) §§0–2 record, and interleaved
with its §3 exactly as noted per step. Host steps run only through the
authorized broker or an `Operator:` handoff with command and rollback —
never from an agent shell. Record UTC times and non-secret receipts on
the execution card; member data, dumps, and logs stay in the restricted
evidence location.

| # | Step | What happens | Evidence recorded |
|---|---|---|---|
| P0 | Guard passes | Dispatch `deploy-production.yml` from `main` with `sha` = full 40-hex commit on `main`. The `sha guard` job requires that SHA's latest `check.yml` run/current attempt completed/success with `ci-ok` (full verdict, not lint-only) and `worker check` bound to that attempt, a successful `deploy-staging` run for the same SHA, the `production` Environment with required reviewers and main-only branch policy, and (after approval) re-verified ancestry on `origin/main` | SHA, CI run/attempt link, staging run link, Environment reviewer count, guard summary |
| P1 | Deploy, fence still active | The job checks out exactly that SHA and runs `wrangler deploy --message <sha>`. The new Worker version serves 100% of traffic, but the singleton record still names the old owner (or `fenced`), so probes must be **refused**, not ready. The `/health` 200 + truthful-`/readyz` gate runs: `/health` 200 proves bindings; `/readyz` 503 parked is the expected healthy-fenced answer, never acceptance | Dispatch link, run summary with SHA + old/new Worker version IDs (the old ID is the rollback `<version-id>`), `/health` 200, `/readyz` 503 `ownership_fenced` |
| P2 | Read control state (no start) | Authenticated `GET /internal/ownership` against the production Worker URL through the production control path (M1.5 client, §Secret surface). This is a read: it returns `{ deploymentId, owner, running }` without starting the Container. Confirm `deploymentId` equals the new Worker version ID from P1, `owner` is the pre-takeover record, and `running=false` | Full GET response ICO (deployment ID, owner epoch/phase/deploymentId, `running=false`); any mismatch is NO-GO |
| P3 | Epoch-checked takeover | Authenticated `POST /internal/ownership` with exactly `{"action":"takeover","expectedEpoch":<epoch from P2>,"actor":"<audit label>"}`. The DO checks the epoch optimistically, persists a **fenced** revocation, awaits native destruction of the old process, verifies `running=false`, then persists `phase=active` for the executing version. Takeover does **not** start the container: a subsequent owned probe does | POST receipt: new epoch = old + 1, `phase=active`, `deploymentId` = executing version, `running=false`, actor/timestamp/old-new epoch audit keys `two-bot:ownership-audit:v1:<epoch>:{fenced,active}` |
| P4 | Verify single start | First owned probe (`/health`, then `/readyz`) starts **one** container. Require `/health` 200, `/readyz` 200 with all components ready and the exact compiled `build_revision`/`build_id` == P0 SHA, `x-two-worker-version` == new version ID, one gateway session, and a keepalive epoch == the P3 epoch. Re-read rollout/Worker control-plane state after the probes | `/health` + `/readyz` bodies (revision match), Worker version header, rollout ID, first READY, session-start budget, keepalive epoch |
| P5 | Hand off to the cutover sheet | Control returns to [cutover-sequence.md](cutover-sequence.md) §3.5/§4: the lead confirms every data/registry/permission gate, then releases producers in the recorded order only after GO and records `T_0` (first production `/readyz` 200 on this revision). `RESUME` stays off: first production boot is the armed one-shot force-fresh IDENTIFY, never imported legacy session state | GO record with `T_0`, producer release order |
| P6 | Watch | The 48-hour watch in [production-deploy.md](production-deploy.md) (checkpoints +15 min, +1 h, +6 h, +24 h, +48 h) with the previous Worker version ID kept in the watch header for the whole watch | Watch rows per the template; ROLLBACK rows repeat the previous version ID |

Ordering constraints (fail closed): never take over before the P1
receipt is validated (staging lesson TOG-12939: the first real run
failed after takeover because the receipt gate had never seen real
Wrangler output); never probe readiness before takeover (a fenced
singleton refuses `/readyz` by design); never treat the P3 receipt as
gateway-ready (it proves ownership, not a session); never release
producers before the lead's GO.

## Every refused state

`ownership.ts` refuses with `{"error":"ownership_fenced","reason":...}`.
The control plane maps `epoch_conflict` to 409; every other fence
refusal is 503 with `cache-control: no-store`. Control-auth failures
are 401/400/405 outside the fence vocabulary. **Every row is NO-GO for
the step that produced it**: freeze, keep maintenance closed, reconcile
state, do not retry with a different credential.

| Refusal (reason / HTTP) | Where it surfaces | Meaning | Operator action |
|---|---|---|---|
| `unauthorized` / 401 | Worker entry, before the DO | Absent/short/mismatched `OWNERSHIP_CONTROL_TOKEN` (floor: 32 chars), or missing `Bearer ` scheme | Stop. Report the 401. Never substitute another credential, pad a short token, or retry with a found key. Provisioning/rotation is a separate governed step |
| `deployment_id_missing` / 503 | `require`/`change`/Worker stamp | `CF_VERSION_METADATA.id` absent or not matching `^[a-zA-Z0-9_-]{1,128}$` | Stop. The version-identity binding is broken; fix the binding, do not hand-edit an ID |
| `deployment_mismatch` / 503 | DO `control` | Ingress deployment header != executing version (stale Worker sharing the namespace, or a forged header overwritten server-side) | Stop. Read state, reconcile which version serves; never copy a header from another version |
| `not_owner` / 503 | `require` on start/probe/keepalive/fetch | No record, `phase=fenced`, record names another version, or caller identity != owner | Expected while fenced (P1) and after a fence (rollback). Do not start, probe-loop, or clear storage. For P4 readiness this means takeover did not happen |
| `storage_invalid` / 503 | `read` | Stored record fails validation (bad epoch chain, `active` with null ID, bad actor/timestamp) | Stop. Never clear or rewrite DO storage or SDK alarms. Escalate to the Director of Engineering |
| `storage_unavailable` / 503 | `read`/`write` | DO storage read/write failed | Stop. A failed write is **not** proof ownership changed. Re-read; reconcile before any further POST |
| `shutdown_unconfirmed` / 503 | `owned` after `onStart` failure | A prior native startup left an unconfirmed gateway; the fence will not admit new work on top of it | Stop. Require control-plane termination evidence; do not start legacy alongside it |
| `stale_keepalive` / 503 | keepalive callback | Keepalive payload deployment/epoch != current owner (old epoch after takeover/fence) | Expected after P3/P-fence: old schedules drain without renewing. If fresh-epoch keepalives refuse, stop — the owner moved under you |
| `epoch_conflict` / 409 | `change` | `expectedEpoch` != current epoch (stale readback, replayed POST, or a concurrent change) | Re-read with GET, reconcile the new state, then decide. Never invent epoch 0 for a non-pristine record and never blind-retry the same epoch |
| `epoch_exhausted` / 503 | `change` | Counter at `Number.MAX_SAFE_INTEGER` | Stop. Escalate; no further ownership change is possible on this record |
| `operation_failed` / 503 | Any non-`OwnershipRefused` throw inside the gate | Unexpected failure (DO reset path, SDK fault) | Stop. Preserve maintenance; escalate with the receipt |
| `invalid ownership change` / 400 | DO `control` | POST body missing/unparseable, over 1 KiB, or failing `parseChange` (bad action, negative/non-integer epoch, bad actor charset) | Fix the caller (M1.5 client bug), re-read state, then re-issue one POST. A 400 never changed state |
| `method not allowed` / 405 | DO `control`, Worker probes | Non-GET/POST on `/internal/ownership`; non-GET/HEAD on `/health`/`/readyz` | Fix the caller. Do not tunnel the intent through another method |
| Client-side: origin/pin refusal (no HTTP) | M1.5 client preflight | URL not the approved production Worker origin, redirect offered, or `PRODUCTION_WORKER_URL` unset/non-`https`/equal to staging | Stop before deploy. Fix configuration; a staging URL or a redirect carrying the bearer is never followed |
| Client-side: `transition not confirmed` (no HTTP) | M1.5 client post-check | POST returned 2xx but `running` still true, epoch != expected + 1, or phase/ID shape wrong | Preserve maintenance. Re-read; do not POST again until the state is reconciled |
| Client-side: `intentionally fenced or uninitialized` (no HTTP) | `deployment-takeover`-style guard | Owner is `fenced`/absent without an explicit release authorization | Requires the explicit release handoff (the production equivalent of staging's `release_fence=true` dispatch input). A routine deploy never unparks it |
| Watch-level: 429/503 storm, session-budget exhaustion, job/data/event/registry triggers | P6 | Budgets in [production-deploy.md](production-deploy.md) §Signal thresholds; stop conditions in [cutover.md](cutover.md) §§Registry swap and Rollback | Freeze writers, evaluate rollback per §Rollback. Budget exhaustion waits for the recorded reset — never spin-restart or rotate the token |

Retry rule (mirrors the staging client): retry **only** 5xx takeover
POSTs inside a bounded window, re-reading the epoch first; a 5xx after
a committed takeover is confirmed with GET, not a second POST. 401,
400, 405, and 409 are never retried blindly.

## Secret and environment surface (GitHub Environments only)

No host-held secrets. No Operator laptop, Coolify env, checked-in file,
chat message, or ad hoc shell holds a production secret at any step.
Every value below travels as a GitHub Environment binding or a
Cloudflare Worker secret provisioned through the governed path, and is
referenced by **name only** in receipts, logs, and cards.

| Item | Home | Scope | Notes |
|---|---|---|---|
| `OWNERSHIP_CONTROL_TOKEN` | Cloudflare Worker secret, `production` environment | Dedicated control bearer, ≥32 chars | Probed at Worker and DO; never forwarded into the bot container; compared by digest; failures log only the fixed reason. Absent/short = 401, not a fallback to another credential |
| Control-token Actions secret | GitHub `production` Environment secret (staging precedent: `STAGING_OWNERSHIP_CONTROL_TOKEN`) | Read by the M1.5 control path inside the `production`-gated job only | Environment secrets override repository secrets, so staging credentials can never satisfy a production call. Provisioning/rotation is a separate CISO/CEO-packet authorization, never part of takeover |
| `CLOUDFLARE_API_TOKEN` / `CLOUDFLARE_ACCOUNT_ID` | GitHub `production` Environment secrets (optional overrides; else repository secrets staging uses) | Wrangler deploy/rollback/version reads | Same scoping rule as above. Auth failures stop without retrying another credential or requesting extra grants |
| `DISCORD_TOKEN`, `DATABASE_URL`, internal-action signing keys | Cloudflare Worker secrets, `production` environment | Forwarded as container env on start | Never logged, never in `wrangler.toml`, never in dispatch inputs. Staging and production bindings must be disjoint; never copy a production binding into staging or vice versa |
| `PRODUCTION_WORKER_URL` | GitHub `production` Environment **variable** (non-secret) | `https://` production Worker URL, must differ from `STAGING_WORKER_URL` | The deploy job and the M1.5 client both refuse when it is unset, non-`https`, or equal to staging |
| `OWNERSHIP_ACTOR` | Run-derived audit label (operator/run ID from the receipt) | Non-secret, charset `^[a-zA-Z0-9_.:@/-]{1,128}$` | Audit attribution only — **not** identity proof. Authentication is the bearer token |
| `expectedEpoch` | Authenticated GET readback, used once | Integer ≥0; 0 only for a never-initialized record | Refresh before **each** POST. A stale/replayed epoch returns 409 |
| POST body | One HTTPS request, ≤1 KiB, no redirects | `{"action":"takeover" or "fence","expectedEpoch":N,"actor":...}` | Takeover targets only the currently executing version; there is no client-supplied target deployment. `fence` parks **all** versions (`deploymentId=null`) |

Enforcement already checked in: `wrangler.toml` declares no
`INTERNAL_ACTIONS_INGRESS` and no staging-only `TWO_*` vars in
`[env.production.vars]`; the env-bindings check fails either, plus any
top-level staging var. The M1.5 client must refuse non-production
origins the way the staging client refuses production ones, and must
never print a secret (names/IDs only, fixed refusal vocabulary).

## Rollback path (per the practiced drill)

Takeover rollback has two coordinated layers; both follow the ordered
checklist in [cutover-rollback-runbook.md](cutover-rollback-runbook.md)
§4 and the full procedure in [cutover.md](cutover.md) §Rollback. The
invariant is **fence before stop, reconcile before legacy start**, with
**zero acknowledged committed writes** as the maximum accepted loss —
restore-to-freeze alone is not a rollback.

1. **Fence the singleton first.** Declare rollback with UTC time and
   reason; freeze all Next/web writers and producers (do not start
   legacy yet). Authenticated `POST {"action":"fence", ...}` with the
   current readback epoch; verify `phase=fenced`,
   `deploymentId=null`, `running=false` from control-plane/log
   receipts. Pause health callers; fence keepalive, admitted fetches,
   `onStart`, and every SDK auto-start path. An unfenced `/health` or
   `/readyz` request can restart Next and is not a stopped-state probe.
   Record rollback freeze `T_r`, final durable watermarks, and a
   restricted Next-data snapshot. If fencing fails, keep maintenance
   closed — never start legacy in uncertainty.
2. **Reconcile before reopening.** Fence registry/permission writers
   (including administrator edits); capture final live definitions,
   guild permissions, and defaults/inheritance before any restoration;
   reconcile every write since baseline over a transactionally
   consistent capture (counts + hashes + conflicts + final watermark),
   classify applied Discord effects from delivery/audit/replay receipts
   (completed effects are not replayed or undone by a restore), approve
   a **reconciled** registry target (never a stale baseline reset), and
   read back every affected guild. Then restore the pinned legacy
   image/config with the reconciled database binding, pass read-only
   legacy preflight, confirm the Next fence is still active and Next is
   stopped, start **one** legacy gateway with its fresh-session
   procedure, and resume producers in order.
3. **Worker-version revert (no DNS change).** This cutover changes no
   DNS records; traffic moves between Cloudflare Worker versions and
   container images. Revert by dispatching the production workflow with
   the previous Worker version ID from the watch header
   (`rollback=<version-id>`, `sha` = that version's commit or any other
   green `main` commit as the message). The same guard and Environment
   approval apply; the workflow fails unless the target serves 100% of
   traffic, then re-runs the `/health` 200 + truthful-`/readyz` gate. A
   version rollback does not rebuild the image or rewind data: when the
   Rust image is the fault, redeploy the known-good reviewed
   fence-capable source/image pair through the full deploy path after
   confirming schema/binding compatibility.
4. **Never roll back to a pre-fence wrapper.** It ignores persisted
   state and can restart an unauthorized gateway. Retain a reviewed
   fence-capable known-good Worker/image pair before rollout, and keep
   the fence active throughout legacy ownership, deployments, and the
   retirement wait.

## Security properties and residual risks

- Single-owner invariant is durable, not advisory: a stopped container,
  a disabled monitor, or a blocked route alone is insufficient, and the
  design never accepts one as a fence.
- Takeover and fence are authenticated, epoch-serialized, audited
  (`actor`/timestamp/old-new epoch persisted per epoch), and bounded
  (1 KiB bodies, no redirects, digest comparison, `no-store`).
- Blast radius is one namespace/singleton: legacy, another namespace,
  or a directly started gateway needs its own containment receipt.
- Residual risks carried forward to M1.5 and B4 (not closed here):
  control-token custody/provisioning (governed path, F6-class);
  per-isolate rate-limit maps rather than a global edge limit (F7);
  log head-sampling 1 in both environments (rejection discipline is
  load-bearing, F4); session-start budget consumption on restart loops;
  and the unresolved B2 soak classification noted in
  [cutover.md](cutover.md) §Preconditions, which this design does not
  re-litigate.

## Contradiction check (against the cutover sheet and staging cards)

- Against [cutover-sequence.md](cutover-sequence.md): P0–P1 satisfy its
  §§0/3.1–3.4 inputs (pinned SHA, staging receipt, guard, deploy
  summary); P2–P4 implement its §3.5 ("release the Next fence
  (authenticated takeover at the current epoch) and start **one** Next
  container") without adding, reordering, or skipping a step; P5–P6
  return control to its §§4–5 verbatim. No step starts a gateway before
  data/registry verification, and no step treats 503 parked as
  acceptance.
- Against the staging-takeover family ([TOG-18964](/TOG/issues/TOG-18964)
  retries): different seam, deliberately untouched. Staging keeps its
  origin pin, its `STAGING_OWNERSHIP_CONTROL_TOKEN` binding, its
  `deployment-takeover` retry loop, and its `release_fence=true`
  explicit-unpark rule. Production uses disjoint bindings, a disjoint
  singleton/namespace, and a disjoint Environment. No shared secret, no
  shared epoch, no cross-environment reuse.
- Against [cutover.md](cutover.md) preconditions: this design adds no
  new precondition and waives none — missing fence rehearsal,
  missing authorized permission route, or missing durable rollback data
  path remains NO-GO.

## What M1.5 must deliver (out of scope here, recorded for carding)

A reviewed production control path (client or workflow step) that pins
the production origin, refuses redirects, reads the control token only
from the `production` Environment binding, enforces the P0–P4 order
with the refused-state table above, prints only names/IDs and fixed
refusal vocabulary, and proves itself with offline fixtures plus a
staging dry-walk — before B4 may reference it. Staging rehearsal
receipts 1–6 in [cutover.md](cutover.md) remain the fence-proving
evidence; production activation evidence belongs to B4, not M1.5.
