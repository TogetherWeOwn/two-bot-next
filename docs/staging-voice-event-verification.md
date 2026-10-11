# Staging voice-event verification

The four catalogued `voice_event` names are emitted to Rust container
stdout only. That stdout is in the Workers Observability `containers` dataset, which
`scripts/container_stdout_read.py` reads (see `docs/startup-diagnostics.md`). The script
keeps each line's message and a fixed field allowlist that does not include `voice_event`,
`op`, `outcome` or `action`: it can find a voice line by its message
(`--contains "voice_operation"`, `"voice_reconcile planned"`, `"voice action dead-lettered"`,
`"voice creator orphan needs manual deletion"`) but cannot show the `voice_event` field. This
page gives the Operator dashboard procedure that reads the field.

## Catalogued names and emission sites

All four live in `crates/bot/src/voice_rooms.rs` as `tracing` fields.
No channel, member, token, body or ID leaves the process in any field
(see `docs/metrics.md`, log-fields list).

| `voice_event` name | Emission site | Level / condition |
| --- | --- | --- |
| `voice_operation` | `crates/bot/src/voice_rooms.rs` (`observe_voice_operation`) | `info` on `success`, `warn` otherwise; carries `op`/`outcome` |
| `voice_reconcile` | `crates/bot/src/voice_rooms.rs` (reconcile plan) | `info`, only when at least one of `delete_enqueued`, `suspended`, `resumed`, `succession_enqueued` is nonzero; carries plan counts |
| `voice_dead_letter` | `crates/bot/src/voice_rooms.rs` (`mark_failed_observed`) | `warn`, only when a queued action exhausts `QUEUE_MAX_ATTEMPTS`; carries `action`/`attempts` |
| `voice_creator_orphan` | `crates/bot/src/voice_rooms.rs` (creator compensation) | `warn` with `outcome = "manual_needed"`, only when compensation delete failed |

Absence of a name in a window is not proof of a regression: `voice_reconcile`
only logs on a nonzero plan, `voice_dead_letter` and `voice_creator_orphan`
only log on failure paths, and `voice_operation` only logs finished room
outcomes. Treat a missing name as "not observed", then check the
corresponding counter below before escalating.

## Why no CI or API read path exists for `voice_event`

- `wrangler logs --env staging` shows Worker/DO tail, not Rust stdout, and
  the pinned Wrangler has no `containers logs` subcommand
  (see `docs/runbook.md`, Logs and keepalive).
- Container stdout is not in the `cloudflare-workers` Workers Logs dataset; it is
  in the `containers` one (see `docs/startup-diagnostics.md`, Reading container
  stdout). `scripts/container_stdout_read.py` queries it and keeps the message, but its
  allowlist drops the `voice_event`, `op`, `outcome` and `action` fields.
- The Containers API exposes applications, versions, rollouts, instances,
  registries and images. It exposes no container-stdout log endpoint.
- `GET /metrics` through the Worker is `404` by design; the only
  off-container metrics path is the authenticated `GET /ops/metrics`
  scrape described in `docs/metrics.md`.
- The `containers` dataset and the script's `--service` and `--contains` filters
  were exercised live, read-only, from an agent run on 2026-10-11, in addition to the
  offline fixtures. The `voice_event` limit above comes from the script's allowlist,
  not from the API.

## What CI verifies today instead

- Code presence: the four `voice_event` literals are pinned in source and
  covered by unit fixtures.
- Serving build: the staging rollout gate proves the intended Worker
  version, immutable image digest, and `/readyz` 200 with the exact
  compiled revision/build ID (see `docs/staging-rollout-gate.md`).
- Activity counters (not event names): the authenticated `/ops/metrics`
  scrape exposes `two_bot_voice_operations_total{op,outcome}`,
  `two_bot_voice_reconcile_actions_total{action}`,
  `two_bot_voice_dead_letters_total{action}`, `two_bot_voice_orphans_total`
  and related gauges. A nonzero counter proves the underlying path ran;
  it does not prove the stdout line was emitted.

## Operator dashboard procedure

Read-only. No staging writes, no new secrets, no Discord actions. Use the
already-authorized Cloudflare connection; an authentication or permission
failure is a stop — report it, do not try another credential.

1. Record the deployment under test: Worker version ID, container image
   digest, build revision/build ID, and deploy finish time from the
   staging deploy run and its rollout receipt.
2. Confirm the serving build first: `curl` the staging `/readyz` and check
   the response carries the same build revision/build ID and all
   components `ready`. A parked or mismatched singleton cannot produce
   meaningful voice-event evidence.
3. Open the affected container's logs in the Cloudflare dashboard for the
   staging application (Workers & Pages, staging Worker, container
   application). Dashboard retention is 7 days.
4. Filter to the window starting at deploy finish plus first-ready time.
   Search for each literal name in turn: `voice_event="voice_operation"`,
   `voice_event="voice_reconcile"`, `voice_event="voice_dead_letter"`,
   `voice_event="voice_creator_orphan"`.
5. Record, per name: present (with one timestamped line reference) or
   absent in the window. For `voice_operation`, also record the observed
   `op`/`outcome` values; for `voice_reconcile`, the nonzero plan counts;
   for `voice_dead_letter`, the `action`; for `voice_creator_orphan`, the
   `manual_needed` outcome.
6. Cross-check absent names against `/ops/metrics` counters for the same
   window before calling it a gap: a zero counter means the path did not
   run, not that logging broke.
7. Write the result into the deployment record: build revision, window,
   per-name present/absent, counter cross-check, and first-ready time.
   Redact tokens, connection strings and member data from the evidence.

## Future options (not authorized here)

If a CI-readable emission proof is wanted later, the established pattern
is the gateway-failure mirror: the keepalive already forwards
`container_gateway_failure` with `phase`/`class` as a Worker
`console.warn` line so it reaches Workers Logs
(`wrangler/src/index.ts`, keepalive). A similar allowlisted Worker-side
mirror for voice counters, or a Logpush/OpenTelemetry export, would need
its own reviewed card, privacy check, and secret/binding authorization.
This page authorizes none of that.

## Sources

- Container stdout read path:
  `docs/runbook.md` (Logs and keepalive),
  `docs/startup-diagnostics.md` (Reading container stdout).
- Metrics names and the authenticated off-container scrape:
  `docs/metrics.md` (log fields, off-container scrape and alert rules).
- Staging rollout provenance and readiness:
  `docs/staging-rollout-gate.md`.
- Cloudflare documentation read for this page:
  [Workers Logs](https://developers.cloudflare.com/workers/observability/logs/workers-logs/),
  [Workers Logpush](https://developers.cloudflare.com/workers/observability/logs/logpush/),
  [Query Builder](https://developers.cloudflare.com/workers/observability/query-builder/).
