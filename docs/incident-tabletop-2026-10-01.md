# Incident tabletop evidence — 2026-10-01

Executor: DevOps & Reliability Engineer.
Source baseline: `8af9b23be40935b97cf29512be3aa45a8b368858`.
Playbooks: [operations runbook](runbook.md#incident-playbooks).

## Scope and result

**Local source walkthrough completed; staging walkthrough blocked at the access
precondition. This is not staging acceptance, an injected outage, or recovery
proof.** No Discord request, SQL/DB probe, restore, migration, deploy, stop,
credential fetch/change, or production action was performed.

The staging target is `two-bot-next-staging` at
`https://two-bot-next-staging.5150.workers.dev`. An existing staging repair
assignment owns the baseline and observability. A separate diagnostics handoff
was cancelled by the operator: lack of container stdout is a staging
observability configuration gap, not a reason to invent another log-access
approval. This walkthrough does not create a duplicate repair assignment.

## Actual bounded observations

| UTC | Operation | Observed result / decision |
|---|---|---|
| 2026-10-01, before HTTP observation | `gh api repos/TogetherWeOwn/two-bot-next/actions/variables/STAGING_WORKER_URL` | HTTP 403, `Resource not accessible by integration`. Stopped this denied metadata operation; no alternative credential. |
| 2026-10-01T04:29:20.718813Z | GET staging `/health`, 20-second timeout | HTTP 403; body `error code: 1010`. Access refusal, **not** a Rust liveness response. |
| 2026-10-01T04:29:20.778316Z | GET staging `/readyz`, 20-second timeout | HTTP 403; body `error code: 1010`. No `components` breakdown; cannot infer gateway or DB state. No retry or bypass. |

At discovery, [deploy run 36814151166](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/36814151166)
for the source baseline was still `in_progress`; earlier deploys
[36813280203](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/36813280203)
and [36808678250](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/36808678250)
were `failure`. These are historical discovery results, not a claim about the
current deployment. No current Worker version/image was verified in this run.

## Non-disruptive local walkthrough

The inputs below are **hypothetical source-contract cases**, not staging logs or
metric samples. They exercise the decision sequence without provider calls.
These are historical findings at the October-1 source baseline, **not current
wiring guidance**: later source implements conditional live Hyperdrive redirects,
a database readiness component and a persistent ownership fence. The preserved
negative claims below must not guide a current incident; use the updated
[incident playbooks](runbook.md#incident-playbooks), confirm the deployed revision,
and record a fresh staging walkthrough before cutover. No later source correction
changes the observations or completes the missing staging acceptance.

| Scenario input | First five minutes / containment decision | Recovery evidence required, not observed here |
|---|---|---|
| Discord reconnect warning, liveness 200, gateway starting | Establish environment and provider incident; retain checkpoint; let Twilight reconnect; no second shard, manual checkpoint deletion, burst retry, or replay of uncertain REST writes. | Ready 200 after committed READY/RESUMED; timestamped gap and a separately authorized staging feature journey. `resume=true` alone is insufficient. |
| Discord 4007/4009 or non-resumable invalid session | Accept code-owned fresh IDENTIFY; do not force RESUME or manufacture a checkpoint. | Ready 200 and singleton session; record IDENTIFY and any event/voice-duration gap rather than claiming complete replay. |
| Neon unavailable at startup or checkpoint commit | Fatal configured gateway failure is not safe offline operation. Retain state/backups; no credential substitution, DB reset, migration, restore, or restart storm. Distinguish direct gateway DB from redirect Hyperdrive. | Correct staging binding/dependency restored by its owner, health/ready 200, no fatal persistence failure, and authorized staging feature evidence. No independent DB-ready component exists. |
| Reported Hyperdrive redirect outage | Check wiring first: current Worker passes an undefined live connector, uses snapshot lookup and drops non-live attribution. It cannot demonstrate a Hyperdrive outage or recovery. Do not route the direct-DB gateway to an invented alternative target. | Deployed wiring provenance and explicit snapshot-only limitation; a future live connector needs its own evidence. No synthetic campaign click or DB probe. |
| Actual staging 403/1010 above | Stop before outage diagnosis: no underlying gateway, DB, or recovery observation exists. Preserve denial and reuse staging repair/evidence route. | Authorized staging response with provenance; do not turn access refusal into a Discord/Neon diagnosis. |

Token containment was reviewed against source, **not executed**. A one-shot
inherited Container `stop()` is not a persistent pause: keepalive and probe
requests can restart it. The playbook must state the missing Worker-side
maintenance gate and owner-only credential action, not invent a public stop URL.

## Offline verification

- `npm --prefix wrangler ci --include=dev`: installed pinned Wrangler 4.143.1
  and Container SDK 0.3.7; 0 audit vulnerabilities.
- `npm --prefix wrangler test`: **86 passed, 0 failed**, including Worker
  auto-start fixtures, local runbook/tabletop file and heading link checks,
  existing command/help guards, and incident metric/literal-log source checks.
- The initial new checks caught a token-heading anchor mismatch and a line-split
  log literal; both were corrected before the final passing run.
- `git diff --check`: passed. No Cargo compilation or DB integration test was
  needed for this docs/offline-test change; no Cargo target was created.
- Link checks resolve repository files and Markdown anchors offline. They do not
  prove external URL availability, deployed commands or operational authority.

## Remaining staging acceptance

After the staging repair restores the baseline, record both Discord and Neon
playbook walkthroughs using an already-authorized staging
observation route. Include UTC, deployed Git SHA/Worker version/image provenance,
non-secret target/binding identification, actual health/readiness breakdown,
sanitized available logs, each decision taken, and verification/stop result.
Use hypothetical outage branches without taking down Discord/Neon, replacing
bindings, or touching tokens. A walkthrough that stops for missing evidence must
remain explicitly incomplete; successful source tests do not fill this gate.
