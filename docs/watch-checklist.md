# 48h post-cutover watch: alert checklist and paging path

Operator-facing single sheet for the 48-hour watch after production cutover.
It enumerates the alerts that already exist, where to read each signal, and
how a page reaches the on-call operator. It installs no monitor, adds no
alert rule, and changes no threshold. Threshold budgets live in
[production-deploy.md](production-deploy.md#signal-thresholds-budgets-and-rollback-triggers)
and [cutover.md](cutover.md#48-hour-watch); when this sheet disagrees with
those, those win and this sheet is the one to fix.

Companion read-only queries (one per signal, no thresholds):
[watch-signal-queries.md](watch-signal-queries.md). Watch log template with
header fields, checkpoint rows and the fixed error-class vocabulary:
[production-deploy.md](production-deploy.md) ("48-hour watch log" section).
Rollback triggers: [cutover.md](cutover.md#rollback-preserve-next-window-writes-before-reopening-legacy).

## 1. Alert inventory (already shipped)

Every row below exists in the current source. Rule ids are the single shared
spelling pinned in Rust (`ALERT_RULE_IDS` in `crates/core/src/evidence.rs`)
and the Worker (`packetFilename` in `wrangler/src/alert-rules.ts`).

| Alert | Fires when | Reads from | Runbook |
|---|---|---|---|
| `container_unready_alert` | keepalive `/readyz` probes fail 10 consecutive samples (≈10 min at default 60 s cadence) | Worker logs (structured JSON event) | [container-readiness.md](container-readiness.md#responding-to-an-alert) |
| `container_unready_recovery` | first ready sample after an alert | Worker logs | same as above |
| `container_keepalive_arm_failed` | keepalive schedule lookup/insert fails (monitoring outage, not a readiness sample) | Worker logs | [container-readiness.md](container-readiness.md) |
| `container_unready_webhook_failed` | alert/recovery webhook POST failed or timed out (type + HTTP status only) | Worker logs | [container-readiness.md](container-readiness.md) |
| `job_stale:<job>` | job's last success older than 2 x its cadence (never-succeeded is ignored) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-job-stale) |
| `job_consecutive_failures:<job>` | 3 failed completions in a row; page includes the latest fixed error class when available | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-job-failures) |
| `rest_429_rate` | 429s above 10% of REST requests between samples (min 10 requests; restarts skip the window) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-rest-429) |
| `db_pool_saturated` | pool at max with zero idle for 3 consecutive samples | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-db-pool) |
| `db_errors` | 3 or more storage-layer errors between samples (restarts skip the window; currently counts send-admission SQL) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-db-errors) |
| `send_admission_blocked` | new send-admission refusals in 3 consecutive windows | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-send-admission-blocked) |
| `voice_failures` | room-op failures > 5% of >= 10 ops between samples, or any new dead-letter/orphan | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-voice-failures) |
| `gateway_missed_events` | any increase of `two_bot_gateway_missed_events_total` between samples (first sample and restarts skip the window) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-gateway-missed-events) |
| `ticker_stale:<job>` | 15 s ticker with no success for more than 10 minutes (never-succeeded is ignored) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-ticker-stale) |
| `receiver_refusals:<family>` | refused website-action outcomes rising in 3 consecutive samples per family (first sample and restarts clear the streak; `other` holds pre-auth refusals) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-receiver-refusals) |
| `dispatch_drops:<lane>` | **ticket**, not page: drops grow in 3 consecutive completed sample windows per bounded lane after a baseline; reactions include fairness refusals, not proof of gateway packet loss | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-dispatch-drops) |

Out of scope for paging (log-only findings, still recorded on the watch log):
gateway session starts, handler-latency quantiles, unban-queue depth via
`moderation_scheduled_unbans` counts (test-container copy or backup artifact
only, never staging/production), and restart counts. See the query pack.

## 2. Dashboards and read paths

- Liveness: `GET /health` on the Worker (200 `{"status":"ok"}` means the
  process answers HTTP; not proof of gateway, database or delivery).
- Readiness: `GET /readyz` (200 when every component is `ready` / 503 with the
  component breakdown: `process`, `gateway`, `database`, `token_invalid`; only
  a `gateway` at `down` or `starting` is parked). Suggested poll cadence 60 s;
  record findings, not every healthy poll.
- Metrics: `GET /ops/metrics` on the Worker with the already-provisioned
  scrape token (never in a PR or log). Container-internal `/metrics` is
  reachable only by the Container DO, never publicly.
- Worker/DO logs: `wrangler logs` tail (7-day retention, unsampled) for
  `container_unready_alert`, `container_unready_recovery`,
  `container_keepalive_arm_failed`, `container_unready_webhook_failed`
  and `two-bot container started|stopped`.
- Rust stdout/stderr: the affected container's logs in the Cloudflare
  dashboard (`listening`, `gateway shard loop started`, `periodic job
  failed`, fatal `error_class` lines). Worker tail is not Rust stdout.
- Watch checkpoints: +15 min, +1 h, +6 h, +24 h, +48 h from `T_0`,
  one GO / EXTEND / ROLLBACK row each on the execution card.

## 3. Paging path

1. The singleton Durable Object's keepalive probes `/readyz` every
   `KEEPALIVE_SECONDS` (default 60) and pulls container `/metrics` via
   `containerFetch` on the same tick.
2. It evaluates the rules in `wrangler/src/alert-rules.ts` against the new
   sample and the previous one, and persists the new alert state in DO
   storage **before** notifying (at most once per transition).
3. Each transition emits one structured log line and, only when the
   per-environment webhook secret is provisioned, one Discord-compatible
   webhook POST with an empty `allowed_mentions.parse`:
   `two-bot-next ALERT <rule> [(last error class: <class>)]: <summary>. Runbook: <deep link>` or
   `two-bot-next RESOLVED <rule> [(last error class: <class>)].` The optional
   class suffix appears only on job-failure transitions when the fixed class
   series is available; missing/older exposition keeps the classless format.
   Messages carry no mentions, credentials, guild/user identifiers or probe bodies.
4. The operator responds per the linked runbook section, records the
   finding on the watch log, and confirms `container_unready_recovery`
   (or the RESOLVED line) before closing the incident.

Delivery limits the watch lead must plan around: without the webhook
secret, monitoring is log-only; a failed/timed-out webhook is not
retried; a crash between persistence and notification can lose that
notification (the structured log line is the primary signal); this
monitor covers a running keepalive loop, not a missing Worker/alarm or
a total monitoring outage — separate external uptime checks are still
needed for those failure modes.

## 4. Pre-watch provisioning check (names only, never values)

Before `T_0`, the operator confirms each item by name with its
provisioner and records the receipt on the execution card:

- [ ] Webhook secret provisioned in the production Worker environment
  (Discord-compatible HTTPS URL, no userinfo, never in `wrangler.toml`,
  a PR or a log).
- [ ] Scrape token provisioned as the production Worker secret backing
  `GET /ops/metrics`; an off-container scrape returns exposition.
- [ ] Readiness threshold confirmed: default (≈10 min) or the
  per-environment override value.
- [ ] No `container_keepalive_arm_failed` in recent Worker logs
  (the loop is actually armed).
- [ ] One test page received end to end (forced job failure showing
  `job_consecutive_failures:<job>` and `(last error class: <class>)` when the
  series is available, within about one keepalive tick; a missing class series
  still pages), then resolved.

## 5. Paging dry-run receipt

Offline synthetic exercise of the exact shipped code path
(`parseExposition` → `evaluateMetrics` → `transitionMessages`), run
2026-10-03 against the working branch; no staging or production system
was touched and no credential was used:

- 429 storm (10→15 429s over a 12-request window): fired
  `rest_429_rate` with the exact webhook line
  `two-bot-next ALERT rest_429_rate: Discord REST 429s exceed 10% of
  requests. Runbook: <runbook deep link>`; quiet follow-up sample
  emitted `two-bot-next RESOLVED rest_429_rate.`
- Pool saturation (max 5, idle 0): silent on ticks 1–2, fired
  `db_pool_saturated` on tick 3 with the exact webhook line.
- Job streak (`audit_retry` 3 consecutive failures): fired
  `job_consecutive_failures:audit_retry`; a never-succeeded job stayed
  silent (parked/just-started exemption held).
- Counter reset (post-restart exposition): skipped the 429 window, no
  false fire.
- Full Worker suite green on the same checkout: 347 tests, 0 failures
  (`npm --prefix wrangler test`).

Still open: the live end-to-end page on staging (needs the provisioned
staging webhook plus an operator-dispatched forced failure) and the
production pre-watch check in §4. Both are follow-up cards on the
cutover execution thread, not blockers on this sheet.

## 6. Known gaps (follow-ups live on the cutover thread)

- Staging/production webhook and scrape-token provisioning state is
  unverified from the workspace; confirming binding names and running
  the live page needs the governed operator path.
- No external uptime check covers a missing Worker/alarm or a total
  monitoring outage; the keepalive only watches a running loop.
- The DB-error counter and send-admission series now page (`db_errors`,
  `send_admission_blocked`); residual: only send-admission SQL reports
  `db_errors` so far, other stores adopt the counter incrementally, and a
  slow error trickle below threshold stays silent by design (job-failure
  rules cover it).
- Webhook delivery is at most once per transition with no retry; a
  crash between persistence and notification loses that page.
- Voice room-operation budgets consume lifecycle outcome signals that
  live on a sibling branch, not on the main line yet.
