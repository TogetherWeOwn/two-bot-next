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
| `container_unready_alert` | keepalive `/readyz` probes fail 2 consecutive samples (≈2 min at default 60 s cadence) | Worker logs (structured JSON event) | [container-readiness.md](container-readiness.md#responding-to-an-alert) |
| `container_unready_recovery` | first ready sample after an alert | Worker logs | same as above |
| `container_keepalive_arm_failed` | keepalive schedule lookup/insert fails (monitoring outage, not a readiness sample) | Worker logs | [container-readiness.md](container-readiness.md) |
| `container_unready_webhook_failed` | alert/recovery webhook POST failed or timed out (type, HTTP status, release, log URL; never the binding or body) | Worker logs | [container-readiness.md](container-readiness.md) |
| `job_stale:<job>` | job's last success older than 2 x its cadence, with a 180 s floor for `event_voice` (never-succeeded is ignored) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-job-stale) |
| `job_consecutive_failures:<job>` | 3 failed completions in a row; page includes the latest fixed error class when available | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-job-failures) |
| `rest_429_rate` | 429s above 10% of REST requests between samples (min 10 requests; restarts skip the window) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-rest-429) |
| `rest_5xx_rate` | 5xx above 10% of REST requests between samples (min 10 requests; restarts skip the window) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-rest-5xx) |
| `rest_admission_breaker_open` | **ticket**, not page: breaker open for 3 consecutive valid samples; one valid closed sample resolves, but does not prove fatal-token recovery | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-rest-admission-breaker-open) |
| `db_pool_saturated` | pool at max with zero idle for 3 consecutive samples | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-db-pool) |
| `db_errors` | 3 or more storage-layer errors between samples (restarts skip the window; send-admission SQL `op="admission"` plus voice actor load `op="other"`) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-db-errors) |
| `send_admission_blocked` | new send-admission refusals in 3 consecutive windows | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-send-admission-blocked) |
| `voice_failures` | room-op failures > 5% of >= 10 ops between samples, or any new dead-letter/orphan | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-voice-failures) |
| `gateway_missed_events` | any increase of `two_bot_gateway_missed_events_total` between samples (first sample and restarts skip the window) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-gateway-missed-events) |
| `gateway_unpaired_disconnect` | disconnects without a RESUMED or fresh READY for 3 consecutive samples (first sample and restarts re-baseline; any recovery growth clears) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-gateway-unpaired-disconnect) |
| `gateway_not_ready` | `/readyz` gateway component `starting`/`down` for 3 consecutive valid samples with no disconnect edge required (first valid sample and restarts re-baseline; any ready sample clears) | keepalive `/readyz` probe | [runbook.md](runbook.md#alert-gateway-not-ready) |
| `ticker_stale:<job>` | 15 s ticker with no success for more than 10 minutes (never-succeeded is ignored) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-ticker-stale) |
| `receiver_refusals:<family>` | refused website-action outcomes rising in 3 consecutive samples per family (first sample and restarts clear the streak; `other` holds pre-auth refusals) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-receiver-refusals) |
| `dispatch_drops:<lane>` | **ticket**, not page: drops grow in 3 consecutive completed sample windows per bounded lane after a baseline; reactions include fairness refusals, not proof of gateway packet loss | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-dispatch-drops) |
| `job_unknown_stale:<job>` | **ticket**, not page: job without a cadence entry with no success for more than 2 hours (never-succeeded is ignored; `session_checkpoint` is event-driven and excluded) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-job-unknown-stale) |
| `gateway_checkpoint_failures` | any increase of `two_bot_gateway_checkpoint_failures_total` summed over every `stage` between samples (first sample is baseline-only; the 75 s runtime linger leaves ~3 s margin over a 60 s tick plus two 6 s probes for a pre-exit scrape) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-checkpoint-failures) |
| `voice_join_fast_burn` | **page**: genuine creator joins burn the 99.5% move-within-5s budget faster than 14.4x in both the 1 h and 5 min windows (each needs at least 10 joins and two bad moves; incomplete startup/restart windows stay inactive) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-voice-join-fast-burn) |
| `voice_join_slow_burn` | **ticket**: genuine creator joins burn the 99.5% move-within-5s budget faster than 6x in both the 6 h and 30 min windows (each uses the same volume and bad-move floors) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-voice-join-slow-burn) |
| `voice_creator_map_stale` | creator-map age past 600 s (two missed 5 min refreshes) after at least one merge; generation 0 never fires, older containers without the gauges hold state | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-voice-creator-map-stale) |
| `community_facts_drain_failures` | **ticket**: summed community-facts drain failures grow in 3 consecutive windows after a baseline; single bursts stay silent, flat/reset resolves | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-community-facts-drain-failures) |
| `event_store_consumer_lag` | **ticket**: consumer lag above 60 s in 3 consecutive valid samples (fractional counts); a valid sample at or below 60 s resolves, missing/invalid samples and failed scrapes preserve a firing ticket | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-event-store-consumer-lag) |
| `error_budget_fast_burn` | **page**: REST error budget burns faster than 12x in both the 15 min and 2 min windows (each needs at least 10 requests and two budget errors; incomplete startup/restart windows stay inactive) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-error-budget-fast-burn) |
| `error_budget_slow_burn` | **ticket**: same budget burns faster than 3x in both the 2 h and 15 min windows (same floors; needs full 2 h history, holds on gaps, covered quiet 15 min resolves) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-error-budget-slow-burn) |
| `eventsub_reconcile_failed` | **ticket**: failed EventSub reconcile passes rise between samples (first sample is baseline-only; a restart sample fires when the new process already counted a failed pass, else holds) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-eventsub-reconcile-failed) |
| `prefix_trigger_verdict_refusals` | **ticket**: automod-verdict refusal of prefix commands rises in 3 consecutive samples (first sample is baseline, flat/reset clears; `other` never counts) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-prefix-trigger-verdict-refusals) |
| `voice_worker_halted` | **page**: halt latch reads 1 (any guild actor halted); a valid 0 clears at once, missing/invalid holds | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-voice-worker-halted) |
| `voice_live_self_heal` | **ticket**: self-heal repairs rise between valid samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-voice-live-self-heal) |
| `voice_live_not_ready` | **page**: live view not ready past 120 s on a connected gateway (promotes the host-watchdog bound; the sibling keeps the age climbing through a hang); at or below clears, missing/invalid holds | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-voice-live-not-ready) |
| `voice_queue_stall` | **ticket**: oldest queued action waiting past 120 s (matches the stuck horizon; the sibling keeps the age climbing through a hang); at or below clears, missing/invalid holds | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-voice-queue-stall) |
| `retention_replay_needed` | **ticket**: retention purge skips needing Operator replay rise between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-retention-replay-needed) |
| `voice_dispatch_timeouts` | **ticket**: dispatch turns past the 60 s observe bound rise between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-voice-dispatch-timeouts) |
| `voice_watchdog_stucks` | **ticket**: watchdog stuck windows rise between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-voice-watchdog-stucks) |
| `voice_halt_recoveries` | **ticket**: halt-latch recoveries rise between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-voice-halt-recoveries) |
| `rest_breaker_opens` | **ticket**: invalid-request breaker opens rise between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-rest-breaker-opens) |
| `rest_global_pauses` | **ticket**: Discord global-pause episodes rise between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-rest-global-pauses) |
| `gateway_latency_dead` | no heartbeat-ACK evidence (NaN or unchanged latency) for 3 consecutive valid samples (first sample is baseline, fresh readings clear at once; co-firing with `gateway_not_ready` means never-connected) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-gateway-latency-dead) |
| `gateway_reconnect_flapping` | successful transport reconnections rise in 3 consecutive samples (single blips stay log-only; first sample is baseline, flat/reset clears) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-gateway-reconnect-flapping) |
| `handler_duration_saturated` | **ticket**: handler p99 past 1 s (tail above 1% with >= 10 completions, >= 2 slow) in 3 consecutive windows | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-handler-duration-saturated) |
| `job_failure_share:<job>` | **ticket**: per-job failures above 10% of recent runs (>= 10 runs, >= 2 failures; sub-floor windows accumulate) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-job-failure-share) |
| `rest_token_invalid` | **page**: bot-token latch reads 1 on any valid sample (credential death; first sample may fire, only a corrected restart clears) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-rest-token-invalid) |
| `rest_refusal_surge` | **page**: guard invalid + refused requests grow 10+ between samples (first sample is baseline-only, a backwards counter re-baselines only that family, quiet windows resolve) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-rest-refusal-surge) |
| `rest_global_responses_stuck` | **page**: global-pause response bodies unresolved for 3 consecutive valid samples (a valid zero resolves; paused/remaining gauges stay rule-free by design) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-rest-global-responses-stuck) |
| `feed_fetch_failures` | **ticket**: summed feed-fetch errors rise between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-feed-fetch-failures) |
| `eventsub_receive_gap` | **ticket**: received EventSub deliveries exceed classified deliveries for 3 consecutive samples (closed gap or joint reset resolves; gaps hold firing) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-eventsub-receive-gap) |
| `eventsub_reconcile_prior_failed` | **ticket**: reconcile prior-failed findings rise between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-eventsub-reconcile-prior-failed) |
| `automod_sanction_refused` | **ticket**: automod sanction refusals rise between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-automod-sanction-refused) |
| `moderation_enforcement_failures` | **ticket**: summed moderation `discord_error`/`storage_error` outcomes rise between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-moderation-enforcement-failures) |
| `voice_rest_stuck` | **ticket**: voice REST in-flight age above 60 s for 3 consecutive samples (at-or-below-bound resolves; gaps hold firing) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-voice-rest-stuck) |
| `voice_global_backoff_capped` | **ticket**: voice global-backoff caps rise between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | `/ops/metrics` scrape | [runbook.md](runbook.md#alert-voice-global-backoff-capped) |

Out of scope for paging (log-only findings, still recorded on the watch log):
single gateway session starts (sustained reconnect flapping pages via
`gateway_reconnect_flapping`), routine handler-latency quantile movement
(sustained p99 saturation tickets via `handler_duration_saturated`),
unban-queue depth via `moderation_scheduled_unbans` counts (test-container
copy or backup artifact only, never staging/production), and restart counts.
See the query pack.

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
  `container_keepalive_arm_failed`, `container_unready_webhook_failed`,
  `metrics_alert`, `metrics_alert_webhook_failed`
  and `two-bot container started|stopped`. A `metrics_alert` line carries
  the ALERT or RESOLVED transition in `content`; with forwarding off it
  is the only metrics-alert delivery.
- Rust stdout/stderr: the affected container's logs in the Cloudflare
  dashboard (`listening`, `gateway shard loop started`, `periodic job
  failed`, fatal `error_class` lines). For DB errors with `op="other"`,
  correlate with `voice actor load failed; retrying with capped backoff`
  (or `voice actor load failed; credential refused, not retrying`): the
  warn is throttled process-wide to once per 300 s, while
  every load failure increments `two_bot_db_errors_total`. Worker tail is
  not Rust stdout.
- Watch checkpoints: +15 min, +1 h, +6 h, +24 h, +48 h from `T_0`,
  one GO / EXTEND / ROLLBACK row each on the execution card.

## 3. Paging path

1. The singleton Durable Object's keepalive probes `/readyz` every
   `KEEPALIVE_SECONDS` (default 60) and pulls container `/metrics` via
   `containerFetch` on the same tick.
2. It evaluates the rules in `wrangler/src/alert-rules.ts` against the new
   sample and the previous one, and persists the new alert state in DO
   storage **before** notifying (at most once per transition).
3. Each transition emits one structured log line (`metrics_alert` for a
   metrics rule) and, only when `OPS_ALERT_FORWARDING = "on"` **and** the
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

The checked-in `wrangler/wrangler.toml` sets `OPS_ALERT_FORWARDING = "off"`
in every environment, including production. This is a config-file fact,
not verification of deployed private bindings or overrides. When forwarding
is off, the `metrics_alert` log event is the only metrics-alert delivery,
even if a webhook secret exists; a secret alone does not enable paging.

Delivery limits the watch lead must plan around: without forwarding on and
the webhook secret, monitoring is log-only; a failed/timed-out metrics
webhook emits `metrics_alert_webhook_failed` and is not retried. A crash
between persistence and notification can lose that notification (the
structured log line is the primary signal); this monitor covers a running
keepalive loop, not a missing Worker/alarm or a total monitoring outage —
separate external uptime checks are still needed for those failure modes.

## 4. Pre-watch provisioning check (names only, never values)

Before `T_0`, the operator confirms each item by name with its
provisioner and records the receipt on the execution card:

- [ ] Delivery mode confirmed by the operator: log-only, or forwarding
  explicitly enabled through the governed operator path. This sheet does
  not turn `OPS_ALERT_FORWARDING` on.
- [ ] For webhook paging, webhook secret provisioned in the production
  Worker environment (Discord-compatible HTTPS URL, no userinfo, never in
  `wrangler.toml`, a PR or a log); forwarding must also be on.
- [ ] Scrape token provisioned as the production Worker secret backing
  `GET /ops/metrics`; an off-container scrape returns exposition.
- [ ] Readiness threshold confirmed: default (≈2 min) or the
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
- The DB-error, send-admission and voice lifecycle rules ship on main
  (`db_errors`, `send_admission_blocked`, `voice_failures`); delivery follows
  §3, not an assumed webhook page. DB-error producers are send-admission
  SQL plus voice actor load; other stores adopt the counter incrementally.
  A slow error trickle below threshold stays silent by design; job-failure
  rules cover failed jobs, not every storage path.
- Webhook delivery is at most once per transition with no retry; a
  crash between persistence and notification loses that page.
