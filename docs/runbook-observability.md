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
  carries the fixed-vocabulary phase/class plus the failing step and the
  SQLSTATE code (`failed_step`/`error_code`), never an SQL error message.
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
sustained disconnect without RESUMED or fresh READY. `rest_5xx_rate` fires when
Discord 5xx responses exceed 10% of REST requests in a window. Read the class
and failed step from the Container's structured `gateway_failed` log line,
which carries `failed_step` and the sanitized `error_code`. The Worker
re-emits the same phase, class, failed step and SQLSTATE code in its
`container_gateway_failure` event, and `/readyz` serves the step plus the code
in `gateway_failure.failed_step`/`error_code` alongside the phase/class. SQL
errors are never emitted verbatim. A PostgreSQL SQLSTATE code appears only when
the producer explicitly extracts it; do not infer one from a generic
`database` class. A missing grant (`42501`) or missing table (`42P01`) is a
deployment/schema/ACL issue for the authorized Operator,
not something to repair with a production query or restart.

Every alert message and alert log line carries the container build revision and
a log-query link; a crash-loop or unhandled exception surfaces as a sustained
`/readyz` red, which alerts after the readiness threshold below.

If `/readyz` remains red, the Worker/DO readiness transition alerts after its
configured consecutive-failure threshold (default approximately two minutes
at the 60-second keepalive cadence). A missing/invalid metrics scrape preserves
existing firing state and does not count as recovery. See the detailed rule
definitions and their alert-response sections in
[the operations runbook](runbook.md#metrics-alerts) and
[metrics reference](metrics.md#off-container-scrape-and-alert-rules).

## Known coverage limits

These are deliberate, visible limits of the current implementation; do not
claim them as shipped acceptance:

- Container tracing is structured JSON, but a single `request_id` is not yet
  propagated end-to-end from Worker through Container, Durable Object, and
  queued work. Do not assume logs can be joined by request ID.
- Release-tagged external error tracking is not configured. Production Workers
  Logs are enabled unsampled; traces remain disabled to avoid recording
  credential-bearing URLs.
- This repository's synthetic workflow is staging-oriented. Do not describe it
  as a passive five-minute production user-flow check unless the production
  schedule and alert delivery have been independently verified.
- Error-rate detection is installed: `rest_5xx_rate` pages when 5xx exceed 10%
  of Discord REST requests between keepalive samples (minimum 10 requests),
  with the rollback evaluation in [Alert: REST 5xx](runbook.md#alert-rest-5xx).
  What is not yet wired is a canary-stage error-rate gate that fails a
  promotion stage automatically: a 5xx burst during promotion pages on-call,
  and the existing per-stage health gates roll back only on their own probe
  failures.
- The shared central gradual-promotion action promotes 5-50-100% with
  per-stage health gates and automatic rollback to the recorded pre-promote
  version. This repository has no caller wiring to it yet, so do not treat
  the existing gated readiness check plus manual rollback as proof of a
  gradual traffic rollout here.

For the full health, metrics, and rollback procedures, see the
[operations runbook](runbook.md), [metrics reference](metrics.md), and
[production deployment guide](production-deploy.md).
