# Incident tabletop evidence — 2026-10-01

Task: [TOG-11150](/TOG/issues/TOG-11150). Executor: DevOps & Reliability Engineer.
Source baseline: `8af9b23be40935b97cf29512be3aa45a8b368858`.
Playbooks: [operations runbook](runbook.md#incident-playbooks).

## Scope and result

**Local source walkthrough completed; staging walkthrough blocked at the access
precondition. This is not staging acceptance, an injected outage, or recovery
proof.** No Discord request, SQL/DB probe, restore, migration, deploy, stop,
credential fetch/change, or production action was performed.

The staging target is `two-bot-next-staging` at
`https://two-bot-next-staging.5150.workers.dev`, named in the existing
[staging repair card](/TOG/issues/TOG-11131) and
[diagnostics handoff](/TOG/issues/TOG-11132). The latter was cancelled by the
operator: lack of container stdout is a staging observability configuration
gap, not a reason to invent another log-access approval. Repair/observability
belongs to the existing repair card; this task does not create a duplicate.

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

| Scenario input | First five minutes / containment decision | Recovery evidence required, not observed here |
|---|---|---|
| Discord reconnect warning, liveness 200, gateway starting | Establish environment and provider incident; retain checkpoint; let Twilight reconnect; no second shard, manual checkpoint deletion, burst retry, or replay of uncertain REST writes. | Ready 200 after committed READY/RESUMED; timestamped gap and a separately authorized staging feature journey. `resume=true` alone is insufficient. |
| Discord 4007/4009 or non-resumable invalid session | Accept code-owned fresh IDENTIFY; do not force RESUME or manufacture a checkpoint. | Ready 200 and singleton session; record IDENTIFY and any event/voice-duration gap rather than claiming complete replay. |
| Neon unavailable at startup or checkpoint commit | Fatal configured gateway failure is not safe offline operation. Retain state/backups; no credential substitution, DB reset, migration, restore, or restart storm. Distinguish direct gateway DB from redirect Hyperdrive. | Correct staging binding/dependency restored by its owner, health/ready 200, no fatal persistence failure, and authorized staging feature evidence. No independent DB-ready component exists. |
| Hyperdrive redirect lookup/write failure only | Separate invite redirect degradation from gateway persistence; fallback may still redirect without attribution. Do not route the gateway to an invented alternative DB. | Configured redirect lookup/delivery evidence separate from gateway readiness; no synthetic click campaign or production DB probe. |
| Actual staging 403/1010 above | Stop before outage diagnosis: no underlying gateway, DB, or recovery observation exists. Preserve denial and reuse staging repair/evidence route. | Authorized staging response with provenance; do not turn access refusal into a Discord/Neon diagnosis. |

Token containment was reviewed against source, **not executed**. A one-shot
inherited Container `stop()` is not a persistent pause: keepalive and probe
requests can restart it. The playbook must state the missing Worker-side
maintenance gate and owner-only credential action, not invent a public stop URL.

## Remaining staging acceptance

After [TOG-11131](/TOG/issues/TOG-11131) restores the baseline, record both
Discord and Neon playbook walkthroughs using an already-authorized staging
observation route. Include UTC, deployed Git SHA/Worker version/image provenance,
non-secret target/binding identification, actual health/readiness breakdown,
sanitized available logs, each decision taken, and verification/stop result.
Use hypothetical outage branches without taking down Discord/Neon, replacing
bindings, or touching tokens. A walkthrough that stops for missing evidence must
remain explicitly incomplete; successful source tests do not fill this gate.
