# Observability and escape detection

This runbook describes the shipped Cloudflare monitoring path for two-bot-next.
It does not authorize production mutations. Production is read-only during
incident triage; use the separately gated production deployment/rollback flow
only after identifying the affected build and following its approval contract.
No Gatus or third-party monitor is used.

## Where signals live

- **Worker and Durable Object logs:** Cloudflare Workers Observability, with
  unsampled invocations and seven-day retention in both named environments.
  `npm --prefix wrangler run logs -- --env staging --format pretty` tails Worker
  and DO logs only; it is not a Rust container log command.
- **Container logs:** Workers Observability's `containers` dataset, or the
  Cloudflare dashboard's logs for the affected Container application. The
  read-only `scripts/container_stdout_read.py --service <application-id>` query
  reads the dataset without starting the Container. See
  [startup diagnostics](startup-diagnostics.md#reading-container-stdout).
- **Readiness:** `/health` indicates the HTTP process responds;
  `/readyz` reports process, gateway, database, token-invalid state and build
  identity. A 503 includes the bounded readiness breakdown. `gateway_failure`
  is a fixed-vocabulary phase/class, not an SQL error message or SQLSTATE.
- **Metrics and alerts:** the Container exposes `/metrics` internally. The DO
  scrapes it on keepalive ticks, evaluates the rules in
  [`wrangler/src/alert-rules.ts`](../wrangler/src/alert-rules.ts), persists
  alert transitions, and writes structured alert events to Worker logs. The
  optional Discord webhook is sent only when `OPS_ALERT_FORWARDING` is exactly
  `on` and `OPS_ALERT_WEBHOOK_URL` is configured. Forwarding defaults off;
  never put the webhook URL in a command, log, screenshot, or report.
- **Deploy identity:** `/readyz` reports the Container's `build_revision` and
  `build_id`; deploy workflows stamp these values and gate on the expected
  revision. The Worker version ID is distinct from the Git SHA. Production
  release tags are created by the successful promotion flow, not by hand.

## From an alert to the responsible change

1. Record the alert's environment, rule/class, timestamp, and runbook link. Do
   not copy webhook URLs, bearer values, member identifiers, message content,
   database URLs, request headers, or raw remote error bodies into the incident.
2. Open the corresponding Cloudflare Workers Observability logs for the
   environment and time window. For Container failures, use the `containers`
   dataset or the dashboard's Container logs; do not rely on the Wrangler
   `logs` tail to show Rust stdout.
3. Read `/readyz` and, when authorized, the read-only `/ops/gateway` summary.
   Capture `build_revision` and `build_id` from the response. Never treat the
   Worker version ID as the Git SHA.
4. In GitHub, compare the observed `build_revision` with the successful
   `deploy-staging` run (staging) or gated `deploy-production` run (production)
   for that exact SHA. The production run summary records the SHA and Worker
   version; staging rollout evidence is in the `staging-rollout-evidence`
   workflow artifact and emitted verification record. Open
   the commit's PR list / commit details to identify the merged change; inspect
   the deployment's exact commit, not merely the newest `main` commit.
5. Compare the last known-good build and the first failing observation. A
   rollback candidate must be a known-good Worker-version/image pair compatible
   with the current schema. Production rollback is a gated action; do not use a
   manual Wrangler deploy or image switch.
6. Record the exact SHA, deployment run, evidence time range, bounded
   `error_class`/failure class, and recovery proof. Keep raw logs in their
   authorized Cloudflare location and link to the relevant dashboard/query from
   the private incident record rather than copying sensitive log contents.

## Gateway failures and SQLSTATE

`gateway_checkpoint_failures` fires on an increase in the checkpoint-failure
counter between metric samples. `gateway_unpaired_disconnect` detects a
sustained disconnect without RESUMED or fresh READY. Read the class and failed
step from the Container's structured `gateway_failed` log line, which carries
`failed_step` and the sanitized `error_code`. The Worker separately writes a
`container_gateway_failure` event with only the phase and class. The `/readyz`
`gateway_failure` field also exposes phase and class only. SQL errors are never
emitted verbatim. A PostgreSQL SQLSTATE code appears only when the producer
explicitly extracts it; do not infer one from a generic `database` class. A
missing grant (`42501`) or missing table (`42P01`) is a deployment/schema/ACL
issue for the authorized Operator,
not something to repair with a production query or restart.

If `/readyz` remains red, the Worker/DO readiness transition alerts after its
configured consecutive-failure threshold (default approximately ten minutes
at the 60-second keepalive cadence). This is not a two-minute guarantee. A
missing/invalid metrics scrape preserves existing firing state and does not
count as recovery. See the detailed rule definitions and their alert-response
sections in [the operations runbook](runbook.md#metrics-alerts) and
[metrics reference](metrics.md#off-container-scrape-and-alert-rules).

## Request-id correlation

One `request_id` joins the Worker, Durable Object, container and queued-work
logs for a single flow. The Worker stamps `x-request-id` on every forwarded
request (honoring a valid inbound 26-char Crockford value, else minting one),
the DO forwards it to the container, and the container receivers honor the
same shape for their request spans, refusal envelopes and queued follow-up
tasks. Anything else in that header (bearer tokens, URLs, free text, or a
missing header) mints a fresh id and is never logged as one. Every container
log line also carries `build_sha` and `release` (the image's git SHA,
`"unknown"` for local builds). Log lines carry the route (`http.request.path`
on request spans, allowlisted) or job (`job` on periodic-job lines) where one
applies, and `error_class` on failure lines.

To trace one id across all hops, reuse the same `x-request-id` for two
staging requests: a malformed one (refused at the Worker edge) and a
well-shaped one with a bogus signature (forwarded Worker -> DO -> container,
refused by the container). Each hop logs only refusals, so the two requests
together put one id in every system without needing a signing secret:

```sh
RID=01ARZ3NDEKTSV4RRFFQ69G5FAV  # any 26-char Crockford id you mint
BASE="https://<staging-worker>/internal/actions"
curl -X POST "$BASE" -H "x-request-id: $RID" \
  -H "content-type: text/plain" -d '{}'
curl -X POST "$BASE" -H "x-request-id: $RID" \
  -H "content-type: application/json" \
  -H "x-two-key-id: probe" -H "x-two-timestamp: 1790000000" \
  -H "x-two-nonce: probe-nonce-0001" \
  -H "x-two-signature: sha256=0000000000000000000000000000000000000000000000000000000000000000" \
  -d '{"action":"announcement.post"}'
```

Then filter Workers Logs for `$RID` (the Worker's `internal_actions_ingress`
refusal) and the `containers` dataset for `$RID` (the container's
`internal action refused` line); both refusal envelopes echo the same id in
`request_id`. The container leg is reachable only through the DO forward, so
its line proves the Worker -> DO -> container header path. The same header
rides public probes (`/health`, `/readyz`) into the container request span.
Queued follow-up tasks inherit the request span, so their lines carry the
same id. Scheduled keepalive ticks and periodic jobs carry their own minted
ids: they join to themselves, not to an inbound request.

## Known coverage limits

These are deliberate, visible limits of the current implementation; do not
claim them as shipped acceptance:

- Worker/DO alert log lines and Discord messages do not yet consistently carry
  the Container Git SHA and a direct log-query URL. Use the build/deploy
  correlation procedure above.
- Worker/DO log lines do not carry the container `build_sha`/`release`: the
  Worker version ID is distinct from the Git SHA. Correlate via the procedure
  above.
- Release-tagged external error tracking is not configured. Production Workers
  Logs are enabled unsampled; traces remain disabled to avoid recording
  credential-bearing URLs.
- This repository's synthetic workflow is staging-oriented. Do not describe it
  as a passive five-minute production user-flow check unless the production
  schedule and alert delivery have been independently verified.
- The shared promotion workflow already has a gated readiness check and
  rollback behavior. Do not treat this as proof of a gradual traffic rollout
  or an error-rate-triggered production rollback.

For the full health, metrics, and rollback procedures, see the
[operations runbook](runbook.md), [metrics reference](metrics.md), and
[production deployment guide](production-deploy.md).
