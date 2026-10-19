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
   environment and time window. Filter `container_gateway_failure` and
   `metrics_alert` lines by `release` (the deployed git SHA) to scope errors
   to that release. Group gateway failures by `class`; group metrics alerts by
   the alert rule in `content`. Error-class grouping applies to metrics alerts
   only when `content` includes a validated last error class, as with
   `job_consecutive_failures`. For Container failures, use the `containers`
   dataset or the dashboard's Container logs; do not rely on the
   Wrangler `logs` tail to show Rust stdout.
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
which carries `failed_step` and the sanitized `error_code`. The Worker re-emits
the same phase, class, failed step and SQLSTATE code in its
`container_gateway_failure` event plus the `release`/`worker_version` tags, and
`/readyz` serves the step plus the code in
`gateway_failure.failed_step`/`error_code` alongside the phase/class. SQL
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

## Request-id correlation

One `request_id` joins the Worker, Durable Object, container and queued-work
logs for a single flow. The Worker stamps `x-request-id` on enabled
`/internal/actions` and `/hooks/twitch` ingress requests and on `/health` and
`/readyz` container probes (honoring a valid inbound 26-char Crockford value,
else minting one). The DO forwards it to the container, and the container
receivers honor the same shape for their request spans, refusal envelopes and
queued follow-up tasks. Anything else in that header (bearer tokens, URLs, free text, or a
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
- Worker/DO request-span and dispatch lines (`internal_actions_ingress`,
  `eventsub_ingress`) carry `release` (the container Git SHA, deploy-stamped
  on the Worker edge and probe-read on the DO relay, `"unknown"` when neither
  has a SHA yet) and the stable `log_url` above; the Worker version ID is
  still distinct from the Git SHA and is never used as one. Join an edge
  refusal to the container's `build_sha`/`release` lines via `request_id`.
  Correlate other Worker/DO activity via the procedure above.
- Release-tagged error grouping is provided by Workers Logs, not an external
  error-tracking service. Every `container_gateway_failure` and `metrics_alert`
  line carries `release` (the container git SHA) and `worker_version` plus the
  `failed_step`/`error_code`/`build_revision`/`log_url` detail. Filter a deploy
  window by `release`; group gateway failures by `class` and metrics alerts by
  the alert rule in `content`. A last error class is available only when
  metrics-alert `content` includes a validated one. Production Workers Logs are
  enabled unsampled; traces remain disabled to avoid recording
  credential-bearing URLs.
- The five-minute user-flow synthetic (`synthetic-user-flow` workflow, see
  [synthetic-user-flow](synthetic-user-flow.md)) defines staging and passive
  production probes, with one alert after two consecutive failures and one
  recovery message per incident. Production remains read-only and never blocks
  a deploy. The workflow's `force_fail` input exercises staging only; do not
  claim live schedule or alert delivery until the operator bindings and matching
  run/artifact evidence have been independently verified.
- Error-rate detection is installed: `rest_5xx_rate` pages when 5xx exceed 10%
  of Discord REST requests between keepalive samples (minimum 10 requests),
  with the rollback evaluation in [Alert: REST 5xx](runbook.md#alert-rest-5xx).
  The canary gate helper `evaluatePromotionGate` (in
  `wrangler/src/alert-rules.ts`) fails a promotion stage only on that 5xx
  firing; a 429-only burst passes. The staging `deploy` job invokes it in the
  `error_rate_gate` step (`wrangler/scripts/promotion-error-gate.mjs`), and a
  fail verdict fails the stage so the existing auto-rollback restores the
  recorded pre-promote snapshot. Coverage limits, stated plainly: live
  enforcement needs the `STAGING_METRICS_SCRAPE_TOKEN` secret, which is not
  provisioned yet — without it the step records a skip (never a silent pass).
  Until then, the `inject_error_rate_spike` dispatch input judges a rehearsal
  5xx firing to prove the fail-to-rollback chain with no traffic. Do not claim
  live 5xx enforcement on staging until a run shows a `live` verdict of `pass`
  with `rest_requests >= 10` in `error-rate-gate.json`: a quieter window
  records `insufficient_data` (exit 0 with a warning), which proves nothing
  and must never be read as a healthy pass. Traffic between container boot
  and the first scrape lands in the baseline and is not judged; only requests
  between the two scrapes count. Production runs the same gate at the end of
  the `production` job against live production `/ops/metrics` (evidence
  `production-error-rate-gate.json`), failing the stage into the same
  auto-rollback; live enforcement needs the `PRODUCTION_METRICS_SCRAPE_TOKEN`
  secret, else a recorded skip, and the `inject_error_rate_spike` dispatch
  input proves the chain with a rehearsal firing.
- The shared central gradual-promotion action promotes 5-50-100% with
  per-stage health gates and automatic rollback to the recorded pre-promote
  version. The caller contract is implemented (`bin/smoke.mjs`, contract-tested
  against a loopback stub with live staging probes without an override only;
  override probes of a non-owner version return 503 `ownership_fenced` on this
  singleton Worker and cannot pass as a canary; passive proof caller in
  `.github/workflows/gradual-promote-proof.yml`
  with every error-rate input explicit — see `docs/gradual-promote-caller.md`),
  but no gradual traffic rollout has run here yet: the proof workflow refuses
  before any Cloudflare call until its isolated synthetic fixture lands, and
  production has no gradual caller. Do not treat the existing gated readiness
  check plus manual rollback as proof of a gradual traffic rollout here.

For the full health, metrics, and rollback procedures, see the
[operations runbook](runbook.md), [metrics reference](metrics.md), and
[production deployment guide](production-deploy.md).
