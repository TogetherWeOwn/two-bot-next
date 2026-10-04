# Container rollback tabletop (offline, read-only)

Offline walk of the container rollback runbook against merged source only.
No rollback, deploy, DNS change, database write, staging probe, or live
backout was executed. Timed backout stays with the live drill cards; voice
rollback triggers are covered by their own tabletop, not this page.

- Revision walked: `96ff8512b406f2d92ec597ecea04b90877af72d1` (2026-10-04)
- Runbook: [cutover-rollback-runbook.md](cutover-rollback-runbook.md)
- One-pager: [cutover-rollback-onepager.md](cutover-rollback-onepager.md)
- Procedure source: [cutover.md](cutover.md) (§§ Tool availability,
  Data copy and verification, Registry swap, 48-hour watch, Rollback),
  [production-deploy.md](production-deploy.md) (rollback dispatch, signal
  thresholds), [runbook.md](runbook.md) (Redeploy and rollback),
  [rollback-delta.md](rollback-delta.md),
  [command-restoration.md](command-restoration.md),
  [cutover-image-inventory.md](cutover-image-inventory.md)
- Method: read the files above plus `crates/bot/src/commands_cli.rs`,
  `crates/core/src/command_restoration.rs`,
  `crates/cutover/src/rollback_delta.rs`,
  `crates/cutover/src/bin/preconditions.rs`,
  `crates/core/src/backup/dump.rs` at the revision above. No command was
  executed against staging or production.

## Trigger walk (runbook §2)

First action is the same for every row: freeze all Next and web writers,
keep the current gateway fenced, investigate. Each trigger below names the
numeric budget in [production-deploy.md](production-deploy.md#signal-thresholds-budgets-and-rollback-triggers).

| Trigger | Runbook section | Expected outcome | Gap / TBD |
|---|---|---|---|
| `/readyz` 503 sustained past 60 s after restart/deploy, no recovery path | §2 Readiness, §3 First-ready gap | Freeze writers, investigate; roll back by checkpoint | TBD live: no live window observed offline; keepalive `container_unready_alert` (≈10 min) is a finding, not acceptance |
| Consecutive starts never reaching 200, or repeated supervisor restarts | §2 Crash loop, §3 Drain / Shutdown drain | Freeze writers, evaluate rollback | TBD live: crash-loop observation needs the drill/window |
| Restart loop consuming session reserve (repeated fresh IDENTIFYs, invalid-session storm, budget exhausted) | §2 Session budget | Investigate; roll back if the gateway cannot hold a session | TBD live: session-start budget reading is a live observation |
| Breaker open, or 429-rate alert across consecutive samples after containment, on a new-revision route | §2 REST | Stop workload, freeze writers; roll back if the new revision caused it | TBD live: alert-rule samples are live |
| Watch-critical job stale (last success older than two cadences) or 3 consecutive failures, caused by this revision | §2 Jobs | Freeze consumer, fix dependency; roll back if the regression shipped here | TBD live: job history is live |
| Durable capture gap over any acknowledged write/effect; uncertain send without recorded disposition | §2 Data/journal, §3 Drain | Freeze writers immediately; gap fails GO | GAP-1/GAP-2: measurement is merged (`rollback-delta`), reverse import is explicitly out of scope; unmeasurable tables need a disposition |
| Unexplained gateway-event gap or duplicated execution vs moderator observation | §2 Events | Freeze writers, evaluate rollback | TBD live: needs moderator record comparison |
| Overdue sanction without named disposition that the restored single consumer cannot reconcile | §2 Moderation | Finding; zero unexplained overdue required for GO | TBD live: unban-sweep/job state is live |
| Auth, intent or permission failure; unmapped registry or ambiguous command-ID map; unexplained permission mismatch | §2 Registry | Abort/rollback path, never a retry loop | TBD live: Bearer access check (`command-restoration.md`) is rehearsed in-memory only; live PUT needs authorized executor |
| Sustained RSS growth vs soak floor pressing placement cap, or any OOM-kill | §2 Resources | Freeze writers, investigate or roll back | TBD live: RSS/placement readings are live; B1 floor (~140 MiB) and ceilings are the comparison baseline |

## Step verdicts (runbook §§4–6)

PASS means the offline procedure plus its named tool/contract is present at
the walked revision. FAIL means no merged-only execution path exists.
TBD means the step is writable as a procedure but its proof needs a live
drill, staging run, or authorized executor — explicitly not executed here.

| Step | Expected outcome | Verdict | Gap / TBD |
|---|---|---|---|
| 1. Declare, freeze writers/producers, verify persisted ownership fence before stopping Next, pause health callers, drain to zero, stop Next, verify terminal state, record `T_r`, watermarks, Next snapshot | Fenced, drained, stopped Next with receipts | PASS | TBD live: fence rehearsal receipts ([cutover.md](cutover.md) Required staging rehearsal receipts) and drain measurement need the staging/live drill |
| 2. Fence registry/permission writers, capture final live definitions, permissions, defaults/inheritance before any restoration | Captures held through read-back; failed capture keeps maintenance closed | PASS | TBD live: `commands diff` is merged (`crates/bot/src/commands_cli.rs`); live capture needs the authorized executor |
| 3. Reconcile every write since the freeze baseline with tested per-table/key mapping, not a bare timestamp filter | Counts, hashes, conflicts, applied watermark recorded; zero acknowledged committed writes lost | FAIL | GAP-1: reverse import stays out of scope (`rollback-delta.md`, `rollback_delta.rs`); GAP-2: unmeasurable tables (documented reasons include `community_scorecard_attempts`, `guild_settings_revision`, `level_role_rewards`, `lfg_roles`, `moderation_channel_executions`, `rank_ladder`, `self_role_exchange_baselines`, `web_contract_meta`, plus wholesale-replaced config tables) have no measured delta |
| 4. Classify applied Discord effects from delivery/audit/replay receipts; never replay or undo completed effects by restore | Uncertain effects reconciled with moderators; leases transfer only after old owners fenced | PASS | TBD live: receipt comparison + moderator reconciliation are live actions |
| 5. Approve a reconciled registry target (never an automatic baseline reset), apply definitions, retain returned IDs, reapply complete override sets, read back every affected guild | Zero unexplained mismatches; mismatch keeps commands frozen with named disposition | PASS | TBD live: reconcile/PUT/read-back rehearsed in-memory (`command_restoration.rs` + fixture suite); live run needs provisioned Bearer access and fails closed without it |
| 6. Restore pinned legacy image/config with reconciled binding, pass read-only legacy preflight, confirm fence active and Next stopped, start one legacy gateway with fresh-session procedure, record first READY | Health, event continuity, internal actions, pending jobs verified before reopening writes | PASS | TBD live: legacy pin warmth through the 48 h watch and preflight/first-READY are live; preconditions tooling is merged (`crates/cutover/src/bin/preconditions.rs`) |
| 7. Resume producers/consumers once in recorded order, compare watermarks/registry, watch recovered service for the measured drill window, announce, record evidence | Recovered monitors target legacy; Next evidence kept and fence stays active | FAIL | GAP-3: no measured drill recovery window on record — the all-warm rollback drill precondition ([cutover.md](cutover.md) Preconditions) is still open, and the §7 rehearsal log row (2026-10-03) was a dry-walk with no live probe |
| 8. Incomplete journal capture or failed reverse reconciliation | Writes stay in maintenance, both data sets preserved, decision brief escalated; never silently accept loss | PASS | None offline; escalation path is to the director of engineering per §1 |
| §5 Cloudflare revert (no DNS change) | Rollback dispatch to previous Worker version ID with 100%-traffic assertion plus `/health` + truthful-`/readyz` gate; never to a pre-fence wrapper | PASS | TBD live: dispatch + gate are live executor actions; previous version ID comes from the watch header at `T_0` |
| §6 Data-compat notes | Shared DB: retain additive schema, validate legacy compatibility, never down-migrate or restore old snapshot over it; allocator gate proven before writers release | PASS | GAP-4: `sequenceMarks` archive marks exist (`crates/core/src/backup/dump.rs`, `dump_file.rs`) for archived tables; the separate-DB copier mapping is still planned tooling, so that path has no merged-only execution |

Overall tabletop verdict: **NEEDS WORK** — steps 1–2, 4–6, 8 and §§5–6
read as coherent, link-consistent procedures at this revision; steps 3 and
7 have no merged-only execution path (findings GAP-1 through GAP-3).

## Findings

- GAP-1: no merged reverse-import. `rollback-delta` answers the sizing
  question read-only inside one repeatable-read-only transaction and
  refuses rather than truncates past the export cap; the reverse import
  is explicitly out of scope. Step 3 cannot run merged-only.
- GAP-2: unmeasurable tables. Tables without a usable timestamp column
  report `unmeasurable` with a reason instead of a count. The rollback
  decision needs a recorded disposition for each one; the runbook does
  not name it.
- GAP-3: no measured drill recovery window. Step 7 bounds the watch by
  "the measured drill recovery window" but no all-warm drill receipt is
  on record, so the bound is undefined until the live drill cards close
  it. Timed backout stays on those cards.
- GAP-4: separate-database path tooling. The allocator gate (per-table
  high-water marks, deleted IDs, sequence semantics, no-collision proof)
  is documented and archive marks exist for backup-covered tables, but
  the forward/reverse copier mapping for a separate legacy database is
  still planned tooling, not a merged command sheet.
- TBD-live (not gaps in the writing): fence receipts, drain/terminal-state
  receipts, alert/job/session/RSS observations, moderator effect
  reconciliation, Bearer permission PUT, legacy preflight/first-READY,
  Worker rollback dispatch, and the 48 h watch log all need the
  authorized executor and the live drill/window. None was attempted here.
