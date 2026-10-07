# Incident tabletop evidence — 2026-10-06 (staging)

Executor: DevOps & Reliability Engineer.
Source baseline: `d111423bd7501e332ce6d1583f352f0028650ca1` (current `main`).
Playbooks: [Discord outage](runbook.md#discord-gateway-or-api-outage) and
[Neon/Hyperdrive outage](runbook.md#neon-or-hyperdrive-outage).
Supersedes the access-blocked [October-1 tabletop](incident-tabletop-2026-10-01.md)
for the Discord and Neon walkthroughs; the token playbook stays containment-only
and was not exercised.

## Scope and result

**Staging walkthrough completed for the Discord and Neon playbooks, with the gaps
listed below. This is a dry run: no outage was injected, no Discord write, SQL/DB
probe, restore, migration, deploy, container stop, ownership change, credential
fetch or change, or production action was performed.** Every observation is a read
against the staging Worker or its metadata. It is not cutover acceptance: the
image provenance and the log/metric gaps below remain open.

Target: `two-bot-next-staging` at `https://two-bot-next-staging.5150.workers.dev`.
Probes send the agent `two-bot-next-staging-rollout/1.0`; the default `curl` or
urllib agent is refused by the edge with HTTP 403/1010, which is not an outage.

## Provenance

| Item | Observed |
|---|---|
| Live Worker version | `6a4624df-01a0-4979-968e-87324616b8f6` (`x-two-worker-version` header; created 2026-10-06T18:40:37Z per `wrangler versions view`) |
| `/readyz` build | `build_revision` `d111423bd7501e332ce6d1583f352f0028650ca1`, `build_id` `37512552064-1` (equals `main`) |
| Deploy run for this head | [37512552064](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/37512552064): Worker deployed as the live version above, then the rollout gate **failed** `application_image_drift` (`application_image=stale`, 13 polls, `instances=active:1,healthy:0`) |
| Last gate-green deploy | [37488619351](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/37488619351) at `0be210e0`, Worker `f1bb2669-68d6-461a-abbf-a4450fb32b5b` |
| Running image digest for this head | **Not verified.** The gate could not confirm the image, and the agent Cloudflare token cannot read the containers API. `/readyz` reporting `d111423` is the only evidence the running process is this build |

## Actual observations

| UTC | Operation | Result |
|---|---|---|
| 23:38:35 | `GET /health` | 200 `{"status":"ok"}` |
| 23:38:36 | `GET /readyz` | 200. Components `process`, `gateway`, `database`, `token_invalid` all `ready`. 12 jobs, every `last_error_class` null and `consecutive_failures` 0; `audit_retry`, `community_scorecard`, `self_role_recovery` parked |
| 23:38:36 | Job `last_success` ages (from the `/readyz` epoch-millisecond fields) | `counter`, `member_unban_sweep`, `scheduled_messages`, `settings` under 1 min; `feeds` 2 min; `rank`, `scheduled_events` 6 min; `inactivity`, `presence_probe` 56 min. No cadence is asserted here |
| 23:39:19 | `scripts/qa_cutover_probes.py --expect-ready` | 5/5 passed: liveness, readiness shape, gateway state, jobs map, no ownership refusal |
| 23:39:19 | Discord status JSON | indicator `minor`, "Partially Degraded Service" (page updated 22:34Z) |
| 23:39:20 | Cloudflare status JSON | indicator `major`, but the page's `updated_at` is 2026-08-27: stale, not usable as a correlation input |
| 23:39:20 | Neon status | The statuspage JSON path returns 404; no machine-readable result. Needs a manual page check |
| 23:39 | `wrangler versions view` for the live version | Metadata only. Secret **names** present: `DATABASE_URL`, `DISCORD_TOKEN`, `GUILD_ID`, `OWNERSHIP_CONTROL_TOKEN`, `OPS_ALERT_WEBHOOK_URL`, the `TWO_*` feature secrets. **No `REDIRECT_DB` Hyperdrive binding.** `REDIRECT_MAPPINGS_JSON` is `[]` and `REDIRECT_FALLBACK_CODE` is empty. No values were read |
| 23:39–23:41 | 80-second `wrangler tail --env staging`, event summary only | Three Worker/DO events for two 60-second keepalive ticks (23:40:52Z, 23:41:52Z), all `GET /readyz`, outcome `ok`, status 200, no log lines, no exceptions |

## Discord playbook walkthrough

| Step | Done / decision | Evidence still missing |
|---|---|---|
| Detection: `/health` against `/readyz` `gateway` | Done above: liveness 200, gateway `ready`. Healthy baseline; no first-failure time exists to record | The Rust literals (`gateway reconnect failed; Twilight will retry`, `durable gateway initialized; shard connecting`, `gateway ready; checkpoint committed`) are container stdout, which no agent path reads (see gaps) |
| 1. Environment, provenance, both health routes | Done: provenance table and both responses saved | Image digest (see provenance); ownership state readback |
| 2. Correlate with provider status | Discord reports a minor degradation while staging gateway is `ready` and keepalive reads 200. Decision per the playbook: note it, do not call it a staging outage, and classify only with reconnect/REST evidence | REST 429/5xx/transport counters and reconnect counters need `/ops/metrics` |
| 3. Let Twilight reconnect; no forced RESUME/IDENTIFY | Walked hypothetically. Reconnect warning with liveness 200: retain the checkpoint, wait. 4007/4009 or non-resumable invalid session: accept the code-owned fresh IDENTIFY | Live `resume=true/false` line and READY/RESUMED commit time |
| 4. Which runtime sends REST; durable admission | Walked hypothetically: a held durable lane or unavailable admission database refuses before HTTP, so check [send-admission alerts](runbook.md#alert-send-admission-blocked) before blaming Discord. No operator reset exists | `two_bot_db_errors_total{op="admission"}` and send-refusal samples |
| Containment | Walked: no breaker reset, burst, second identity or redeploy for a provider outage. Bot-wide isolation goes through the token playbook's Worker-side fence | Not exercised: the fence is a gated action, not a tabletop step |
| Recovery verification | Walked: `/health` 200, `/readyz` 200 with `process` and `gateway` ready after committed READY/RESUMED, one session, at least two keepalive intervals clean. Today's baseline already shows the keepalive pair at 60 s spacing, both 200 | A feature journey under separate staging E2E authorization |

## Neon / Hyperdrive playbook walkthrough

| Step | Done / decision | Evidence still missing |
|---|---|---|
| Detection: `/readyz` `database` component | Done: `database=ready` (bounded two-second ping against the Store pool) and `token_invalid=ready`. The `jobs` map shows database-backed jobs (`counter`, `rank`, `scheduled_events`) with recent successes and no `database` error class | `two_bot_db_pool_*` gauges and `two_bot_job_last_success_timestamp_seconds{job}` need `/ops/metrics` |
| 1. Identify the affected dependency from non-secret metadata | Done: the deployed Worker carries `DATABASE_URL` as a secret name, matching the dedicated bot database in [staging configuration](staging-soak.md#provisioning-operator-once). The value was not read | Binding identity was confirmed by name only; the playbook's "confirm the deployed binding" needs the dependency owner |
| 2. Save health, logs, provider status | Health saved. Neon status needs a manual page check (no JSON); Cloudflare's JSON is stale | Sanitized startup/checkpoint logs (container stdout) |
| 3. Persistence failure is unsafe operation | Walked: a failed gateway DB/checkpoint ends the gateway task and supervision exits; no offline spool, no in-memory continuation. A failed ping would show `/readyz` 503 with `database` `down` while `gateway` stays `ready` | None today: no failure exists to observe |
| 4. Redirect wiring | **Observed: no `REDIRECT_DB` binding on the deployed staging Worker**, so the redirect store is in its snapshot-only branch (snapshot is `[]`, no fallback invite). Per the playbook, record this as the configured boundary, not an outage; the Hyperdrive outage branch does not apply to current staging. No campaign click was generated | Behaviour of a live-lookup failure cannot be exercised until a binding exists |
| Containment | Walked: the dependency owner repairs the target; no grant change, URL swap, migration, checkpoint deletion, restore or restart storm | n/a |
| Recovery verification | Walked: owner confirms binding, then 200/200 with the component breakdown for two keepalive intervals, then a separately authorized journey for persisted outcome. No SQL test or restore as verification | A feature persistence journey |

## Gaps found

1. **Ownership status is not readable by this role.** The Worker holds
   `OWNERSHIP_CONTROL_TOKEN`; the engineer environment does not, and the runbook
   forbids substituting another credential. The "read ownership state" branch
   (for a `503 ownership_fenced` response) therefore cannot be walked live from
   an agent run. Route it through the approved staging control path.
2. **No agent path reads container stdout.** Wrangler 4.147.0 has no
   `containers logs`, and `tail` shows only Worker/DO events. The Rust log
   literals the playbooks cite are verified by source tests, not by a live line.
3. **`/ops/metrics` was not used.** It needs the metrics scrape token, may start
   the container, and is not a no-start readback. The metric names are verified
   against the registry by source test only; no live scrape was compared.
4. **Image provenance for this head is unproven** (deploy gate red on
   `application_image_drift`, tracked separately). Use the last gate-green deploy
   until it is accepted.
5. **Redirect live path has no staging coverage** because the Hyperdrive binding
   does not exist; a live-lookup or click-insert failure cannot be rehearsed.

## Offline verification

- `npm --prefix wrangler ci --include=dev`: pinned Wrangler 4.147.0 installed.
- `node --import ./test/cloudflare-loader.mjs --test test/runbook.test.ts` from
  `wrangler/` on `d111423`: 21 passed, 0 failed. It re-checks every cited log
  literal and metric family against the source and resolves all local links.
- Links in this page are checked offline by the same test file; they resolve
  repository files and anchors, not external URL availability.
