# T0 decision input pack (offline doc index)

The cutover lead opens this page on T0. It is an **index only**: every row
points at the procedure that owns the content. No step here is approval to
execute; authority, thresholds and the GO/NO-GO rules live in the linked
docs. When this index and a linked doc disagree, the linked doc wins and
this index is the one to fix.

Freshness cut: `96ff8512` (`origin/main`, 2026-10-04). Each row names the
last commit that touched its target, verified with
`git log -1 --format='%H %ad'` on this checkout.

## 1. Forward sequencing runbook

| Entry | Pointer | Fresh (SHA / date) |
|---|---|---|
| Forward order: freeze, migrate, deploy, verify, watch handoff | [cutover-sequence.md](cutover-sequence.md) | `cc349ed5`, 2026-10-03 |
| Safety contract, abort triggers, full rollback procedure | [cutover.md](cutover.md) | `2fe1e044`, 2026-10-02 |
| Pre-flight go/no-go sheet (all must pass) | [cutover-preflight-checklist.md](cutover-preflight-checklist.md) | `07bc8fe4`, 2026-10-03 |
| Deploy mechanics, dispatch SHA rule, watch log | [production-deploy.md](production-deploy.md) | `94c5d427`, 2026-10-03 |

## 2. Rollback runbook and one-pager

| Entry | Pointer | Fresh (SHA / date) |
|---|---|---|
| Ordered rollback checklist: decider, triggers, time bounds | [cutover-rollback-runbook.md](cutover-rollback-runbook.md) | `aae26c1c`, 2026-10-03 |
| Last-good release pin, backout order, verification | [cutover-rollback-onepager.md](cutover-rollback-onepager.md) | `9b0f8c37`, 2026-10-04 |

## 3. Migration ledger

| Entry | Pointer | Fresh (SHA / date) |
|---|---|---|
| Numbering and checksum guard | [migrations.md](migrations.md) | `fca9a281`, 2026-09-30 |
| Pinned migration list | `migrations.lock` (repo root) | `21879816`, 2026-10-03 |
| Next-window delta sizing (read-only) | [rollback-delta.md](rollback-delta.md) | `c765421e`, 2026-10-02 |

## 4. Smoke contract

| Entry | Pointer | Fresh (SHA / date) |
|---|---|---|
| T0 acceptance expectations (`/readyz`, gateway, command surface, denial copy) | [t0-acceptance-smoke-contract.md](t0-acceptance-smoke-contract.md) | `c1ac049e`, 2026-10-03 |
| Machine-readable companion | [t0-acceptance-smoke-contract.json](t0-acceptance-smoke-contract.json) | `c1ac049e`, 2026-10-03 |
| Blank run-record form (filled live, offline only) | [smoke-run-record.md](smoke-run-record.md) | `70a0cb9f`, 2026-10-04 |

Note: the smoke contract cites `docs/smoke-expected-responses.md`, which is
absent from `origin/main` at this cut. The run-record form above is the
real, present artifact; the missing expectations file is flagged for its
owning slice, not filled in here.

## 5. Dashboard queries

| Entry | Pointer | Fresh (SHA / date) |
|---|---|---|
| Copy-paste queries per cutover panel, warn/page levels | [cutover-dashboard-queries.md](cutover-dashboard-queries.md) | `84154cb1`, 2026-10-04 |
| Read-only watch-signal queries (one per signal, no thresholds) | [watch-signal-queries.md](watch-signal-queries.md) | `ca3f4c77`, 2026-10-04 |

## 6. Watch spec and run record

| Entry | Pointer | Fresh (SHA / date) |
|---|---|---|
| 48h watch alert inventory and paging path | [watch-checklist.md](watch-checklist.md) | `1d156ca4`, 2026-10-04 |
| Watch handover sheet | [cutover-watch-handover.md](cutover-watch-handover.md) | `d6368636`, 2026-10-04 |
| Watch ack owners | [watch-ack-owners.md](watch-ack-owners.md) | `927a3887`, 2026-10-04 |
| 48-hour watch log template | [production-deploy.md](production-deploy.md) ("48-hour watch log") | `94c5d427`, 2026-10-03 |

## 7. Freeze and comms notices

The freeze cadence and the announce points live in the safety contract,
not in a standalone notice file:

| Entry | Pointer | Fresh (SHA / date) |
|---|---|---|
| T-minus checklist: T−24 h maintenance-window announce, T−60 min drain record, T−15 min read-only preflight | [cutover.md](cutover.md) (T-minus checklist) | `2fe1e044`, 2026-10-02 |
| Freeze-and-drain procedure (T_f), moderator coverage | [cutover.md](cutover.md) (Freeze and drain) | `2fe1e044`, 2026-10-02 |

## How to use on T0

1. Confirm every row above still matches its target (one `git log -1`
   per file); any drift is NO-GO until this index is refreshed.
2. Walk section 1 top to bottom; on any failed gate, switch to section 2.
3. Fill the section 4 run-record and the section 6 watch log as the
   cutover proceeds; post member data only to the restricted evidence
   location, never to public GitHub.
