# Internal metrics

The Rust process exposes `GET /metrics` on its existing `LISTEN_ADDR` listener.
This is **internal-only**, unauthenticated operational data: scrape only from the
container/private network. Do not expose the container port publicly or route it through a
public ingress; the only off-container path is the authenticated
`/ops/metrics` route below. Both the Worker entrypoint
and the Container DO refuse `/metrics`; the Worker also reserves `/metrics`,
`/metrics/*` and canonical case/encoding/slash aliases before
invite-campaign lookup. No Prometheus server exists; see the off-container path below.

Format: Prometheus text 0.0.4, `text/plain; version=0.0.4; charset=utf-8`, `no-store`.
Counters reset when the process restarts; timestamps use Unix seconds. Missing
heartbeat latency is `NaN`, not a fabricated zero. Scrapes perform no SQL and
never acquire a database connection. Pool gauges sample SQLx bookkeeping, not
DB reachability; size/idle can change between reads under concurrent traffic.

| Metric | Meaning |
| --- | --- |
| `two_bot_gateway_latency_seconds` | Last heartbeat round-trip from Twilight's completed ACK sample |
| `two_bot_gateway_reconnects_total` | New HELLOs after the first HELLO in the running loop (successful transport reconnections, not failed dial attempts) |
| `two_bot_gateway_resumes_total` | Received RESUMED dispatches |
| `two_bot_gateway_disconnects_total` | Observed transport losses funnelled through the shard supervisor (reconnect failures, close frames, invalid sessions, cold-resume IDENTIFY). Every disconnect must pair with a later RESUME or fresh READY in the same window; an unpaired disconnect means the gateway never came back |
| `two_bot_gateway_missed_events_total` | Dispatches Discord assigned but this process never received (sequence gaps inside one session). Any nonzero increase over the watch window fails the zero-missed-events acceptance |
| `two_bot_gateway_events_total{event}` | Received dispatches, including replays/duplicates, plus heartbeat ACKs and closes; fixed type allowlist, remainder `other` |
| `two_bot_handler_duration_seconds` | Cumulative histogram over nonduplicate dispatch parse/pipeline/durable commit, including failures; seconds |
| `two_bot_rest_requests_total{route,result}` | Executor HTTP sends, including retries; result `2xx`, `3xx`, `4xx`, `429`, `5xx` at response headers or `transport` (failure/cancellation/timeout before headers); later body failures do not hide 429/5xx |
| `two_bot_db_pool_configured` | Whether gateway initialization has registered a pool |
| `two_bot_db_pool_connections` | Current pool size |
| `two_bot_db_pool_idle_connections` | Current idle connections |
| `two_bot_db_pool_max_connections` | Configured maximum |
| `two_bot_db_errors_total{op}` | Storage-layer failures; `op` is `admission` (send-admission SQL) or `other` (every other store until its op joins the allowlist) |
| `two_bot_send_admissions_total{outcome}` | Send-admission `admit()` decisions; `outcome` is `admitted`, `blocked`, `storage_error` (also counted in `two_bot_db_errors_total{op="admission"}`) or `other` |
| `two_bot_job_runs_total{job,outcome}` | Completed attempts; outcome is `success` or `failure` (including returned errors, timeouts and isolated panics) |
| `two_bot_job_last_success_timestamp_seconds{job}` | Last successful completion time in Unix seconds; zero means no success recorded |
| `two_bot_job_consecutive_failures{job}` | Failed completions since the last success; resets to zero on success |
| `two_bot_voice_operations_total{op,outcome}` | Finished room create/move/delete outcomes; `op` is `create`, `move` or `delete`, `outcome` is `success`, `category_full`, `discord`, `persistence` or `cancelled`; retries and 429 backoffs are not outcomes |
| `two_bot_voice_reconcile_actions_total{action}` | Reconcile plan sizes; `action` is `delete_enqueued`, `suspended`, `resumed` or `succession_enqueued` |
| `two_bot_voice_dead_letters_total{action}` | Queue writes that exhausted `QUEUE_MAX_ATTEMPTS` (10); `action` is `create`, `move`, `delete`, `companion`, `ownership`, `kick`, `rename`, `limit` or `other` |
| `two_bot_voice_tracked_rooms` | Rooms tracked in memory; compare with live Discord channels for ghosts |
| `two_bot_voice_compensation_pending` | Tracked rooms awaiting compensating delete after a failed write |
| `two_bot_voice_orphans_total` | Untracked creator-channel orphans needing manual deletion after failed `/create` compensation |

## Job coverage and outcomes

The supervisor records all three job metrics centrally after each completed
attempt. Individual periodic jobs need no instrumentation. The current scheduled
labels are `counter`, `rank`, `scheduled_events`, `presence_probe`,
`community_scorecard`, `inactivity`, `audit_retry`, `self_role_recovery` and
`scheduled_messages` (the 15 s scheduled-message ticker). The community, audit,
recovery and ticker names may be parked by configuration.
All allowlisted series are exposed from process startup at zero, even before the
first run. A zero success timestamp does not distinguish a parked, never-started,
still-running or always-failing job; use `/readyz` job status for that distinction.

Starts, skipped busy deadlines and shutdown cancellation are not completed
outcomes. A returned `Ok(())` is a success even when the job is a gated/no-op tick;
these counters measure scheduler health, not business events or changed rows.
Failures preserve the previous success timestamp. The supervisor's `/readyz`
status uses milliseconds; metrics convert the same completion sample to seconds.
Errors and panic payloads are never labels.

The fixed job allowlist also retains `session_checkpoint`, `invite_snapshot` and
`other`. `session_checkpoint` records successful durable gateway commits through
`job_success`, independently of the periodic supervisor; its success counter is
not a scheduled-job count and its failures are not instrumented here.
`invite_snapshot` remains zero until a real caller records a completion. Unknown
job names share the `other` counters, timestamp and failure streak; an unknown
success resets that shared streak. Add new scheduled names to the compile-time
allowlist, never to a dynamic label map. The supervisor fixture checks the current
website/community registration name catalogs against that allowlist.

This change does not add a feed/roster scheduler or new jobs. Executor calls
already record REST metrics wherever the executor is used; other REST clients
are not silently claimed as covered. Off-container scraping and alerts remain
separate work.

## Scrape contract

- Scrape path: `GET /metrics` on the existing `LISTEN_ADDR` listener
  (default `0.0.0.0:8080`), served by the same router as `/health` and
  `/readyz`. No dedicated port, auth, or query parameters.
- Success status is `200` with the full exposition body, even with no
  gateway and no database. There is no `503` or empty-body case.
- Headers, byte-exact: `Content-Type: text/plain; version=0.0.4;
  charset=utf-8` and `Cache-Control: no-store`.
- Body: Prometheus text exposition 0.0.4. Every family is preceded by its
  `# HELP` / `# TYPE` lines, one sample per line, body ends with a trailing
  newline. All families render from process start at zero/`NaN`, before any
  gateway event or job completion.

## Label allowlists (stable)

Renaming a metric, label, or allowlisted value breaks scrapers; new values
are added to these compile-time lists in `crates/core/src/metrics.rs`, never
as dynamic labels.

- `two_bot_gateway_events_total{event}` — `event` is one of `READY`,
  `RESUMED`, `GUILD_CREATE`, `GUILD_DELETE`, `GUILD_UPDATE`,
  `GUILD_MEMBER_ADD`, `GUILD_MEMBER_REMOVE`, `GUILD_MEMBER_UPDATE`,
  `MESSAGE_CREATE`, `MESSAGE_UPDATE`, `MESSAGE_DELETE`,
  `VOICE_STATE_UPDATE`, `INVITE_CREATE`, `INVITE_DELETE`,
  `INTERACTION_CREATE`, `HEARTBEAT_ACK`, `GATEWAY_CLOSE`, `other`.
- `two_bot_rest_requests_total{route,result}` — `result` is one of `2xx`,
  `3xx`, `4xx`, `429`, `5xx`, `transport`. `route` is one of the fixed
  executor route templates (`GET /channels/:channel`,
  `GET /channels/:channel/messages`, `GET /guilds/:guild`,
  `GET /guilds/:guild/members`, `GET /guilds/:guild/scheduled-events`,
  `DELETE /guilds/:guild/bans/:member`,
  `DELETE /channels/:channel/permissions/:overwrite`,
  `PUT /applications/:application/commands`,
  `PUT /applications/:application/guilds/:guild/commands`,
  `POST /interactions/:interaction/:token/callback`,
  `POST /channels/:channel/messages`,
  `DELETE /channels/:channel/messages/:message`,
  `DELETE /guilds/:guild/members/:member`,
  `PUT /guilds/:guild/bans/:member`,
  `PATCH /guilds/:guild/members/:member`, `PATCH /channels/:channel`,
  `PUT /channels/:channel/permissions/:overwrite`,
  `POST /channels/:channel/messages/bulk-delete`,
  `PUT /guilds/:guild/members/:member/roles/:role`,
  `DELETE /guilds/:guild/members/:member/roles/:role`,
  `POST /guilds/:guild/scheduled-events`,
  `PATCH /guilds/:guild/scheduled-events/:event`,
  `DELETE /guilds/:guild/scheduled-events/:event`, `other`).
- `two_bot_job_runs_total{job,outcome}`,
  `two_bot_job_last_success_timestamp_seconds{job}` and
  `two_bot_job_consecutive_failures{job}` — `job` is one of
  `invite_snapshot`, `session_checkpoint`, `counter`, `rank`,
  `scheduled_events`, `settings`, `presence_probe`, `community_scorecard`,
  `inactivity`, `audit_retry`, `scheduled_messages`, `other`; `outcome` is
  `success` or `failure`.
  `session_checkpoint` records successful durable gateway commits; zero means
  never run. `audit_retry` is the audit supervisor's 30 s retry sweep.
  `settings` is the DB-only 15 s `guild_settings` version poll.
- `two_bot_voice_operations_total{op,outcome}` — `op` is `create`, `move`
  or `delete`; `outcome` is `success`, `category_full`, `discord`,
  `persistence` or `cancelled`. `Rejected` status/code values never become
  labels; all store variants share `persistence`.
- `two_bot_voice_reconcile_actions_total{action}` — `action` is
  `delete_enqueued`, `suspended`, `resumed` or `succession_enqueued`.
- `two_bot_voice_dead_letters_total{action}` — `action` is `create`, `move`,
  `delete`, `companion`, `ownership`, `kick`, `rename`, `limit` or `other`.
- `two_bot_db_errors_total{op}` — `op` is `admission` or `other`. Recorded
  by `Metrics::db_error`; send-admission SQL (admit/extend/complete storage
  failures) reports as `admission`, and failed voice actor store loads
  report as `other`.
- `two_bot_send_admissions_total{outcome}` — `outcome` is `admitted`,
  `blocked`, `storage_error` or `other`. Recorded once per `admit()`
  decision by the Postgres admission gate; failed `complete()`/`extend()`
  storage writes count only in `two_bot_db_errors_total`.
- Log fields (coordinated with blocked structured-log work, which owns JSON
  formatting): `voice_event="voice_operation"` with `op`/`outcome`,
  `voice_event="voice_reconcile"` with plan counts,
  `voice_event="voice_dead_letter"` with `action`/`attempts`, and
  `voice_event="voice_creator_orphan"`. No channel, member, token, body or
  ID leaves the process in any label or field.
- Worker log fields (Workers Logs only, never a metric):
  `event="container_gateway_failure"` with `phase`/`class`, emitted by the
  Container DO keepalive in `wrangler/src/index.ts` once per tick while the
  gateway task is failing. `phase` is `durable_gateway`; `class` is one of
  `store_unavailable`, `gateway_pool_connect_failed`, `checkpoint_load_failed`,
  `onboarding_gates_invalid`, `onboarding_init_failed`,
  `custom_commands_init_failed`, `milestones_load_failed`,
  `automod_config_invalid`, `automod_executor_failed`, `raid_executor_failed`,
  `gateway_runtime_failed` or `gateway_task_panicked` (every `FailureClass`
  variant in `crates/bot/src/gateway_failure.rs`). Only `[a-z0-9_]{1,32}`
  tokens are ever logged; anything else is dropped. See
  [startup diagnostics](startup-diagnostics.md#self-diagnosing-gateway-failures-readyz-gatewayfailure).
- `two_bot_handler_duration_seconds` histogram buckets (`le`, seconds):
  `0.001`, `0.005`, `0.01`, `0.05`, `0.1`, `0.5`, `1`, `5`, `+Inf`, plus
  `_sum` and `_count`.

## Readiness breakdown (`/readyz`)

- `GET /readyz` on the same listener: `200` when every listed component
  reports `ready`, otherwise `503`. `starting` and `down` both count as not
  ready.
- The body is JSON, never a bare error string:
  `{"components": [["process", "ready"], ["gateway", "down"]], "jobs": {...}}`.
  Component statuses serialize lowercase (`ready`/`starting`/`down`), so a
  degraded response names the failing component (e.g. `"gateway"` with
  `"down"` while connecting/reconnecting reports `"starting"`).
- The `jobs` map is informational supervisor status; it does not change the
  readiness code. Supervisor outcomes and alert rules are separately owned
  and not part of this contract.

## Cardinality and memory

All retained metric values are fixed-size arrays. Only compile-time allowlisted
labels reach exposition. REST paths are matched to templates, never copied as
labels: no guild/member/channel IDs, tokens, query strings, visitor data or message
content. Unknown routes/events/jobs collapse to `other`. Histograms have eight
fixed finite buckets and `+Inf`, sum and count. A scrape allocates a bounded-size
text response; observations retain no input string.

`crates/core/examples/metrics_rss.rs` fills every series, submits 100,000 unique
unknown labels, then renders 10,000 scrapes. It reports Linux `/proc/self/status`
RSS before/after and sampled peak (not a high-frequency peak trace), registry
bytes and exposition bytes. CI runs this offline fixture with a 4 MiB incremental
RSS guard and a 256 MiB total fixture guard. The result measures instrumentation
cost, **not** a whole-bot loaded-guild/cache RSS or a staging soak. Hosted CI output
is the measurement receipt; no local Rust build was permitted because the
controller's bounded cache pool was missing at implementation time.

## Verification and sources

- Core unit tests: cumulative histogram, unique series, finite label sets,
  hostile labels, saturating job counters, status groups and missing latency.
- Supervisor fixtures (paused Tokio time, local `Metrics` registries): first
  success for every website/community registration, seconds conversion, returned
  failures, preserved success timestamps, streak reset, timeout/future/factory
  panics collapsing to `other`, and shutdown cancellation producing no outcome.
  Paused timers follow Tokio's `advance` contract; tests yield between jumps:
  <https://docs.rs/tokio/1.53.1/tokio/time/fn.advance.html>.
- Server tests: the existing router returns `/metrics` 200 with the expected
  names/content type; lazy authorized test-pool bookkeeping requires no DB I/O.
- Worker/DO fixture tests: GET/HEAD/POST metrics routes are 404, do not fetch/start
  a container, and cannot become a configured invite redirect — including
  canonical case/encoding/slash aliases (`/METRICS`, `/%6detrics`, `//metrics`,
  `/metrics/*`); near-miss slugs like `metricsfoo` still resolve.
- Prometheus wire contract:
  <https://prometheus.io/docs/instrumenting/exposition_formats/#text-format-details>
- SQLx pool gauges:
  <https://docs.rs/sqlx/0.9.0/sqlx/struct.Pool.html#method.size>
- Twilight heartbeat samples:
  <https://docs.rs/twilight-gateway/0.17.1/twilight_gateway/struct.Latency.html#method.recent>
- Axum router composition follows the existing router/state pattern (0.8.9 in
  `Cargo.lock`); no middleware or additional dependency is introduced.

## Off-container scrape and alert rules

Chosen path: the Container Durable Object (the only caller that can reach the
container-internal listener) pulls `/metrics` via `containerFetch` on every
keepalive tick, evaluates the checked-in rules and records structured transitions
in logs and DO storage. Forwarding to the optional `OPS_ALERT_WEBHOOK_URL` Discord
webhook requires exactly `OPS_ALERT_FORWARDING = "on"`; the default is `"off"`.
Both readiness and metrics share the [Discord-only destination validator and
non-destructive disable switch](container-readiness.md#threshold-and-notifications).
No Prometheus server, no new infrastructure.

- Authenticated pull: `GET /ops/metrics` on the Worker with
  `Authorization: Bearer <METRICS_SCRAPE_TOKEN>`. The token is an optional
  Worker secret (never a plain var). Unset → `404`; missing or wrong bearer →
  `401` (compared via SHA-256 digests); non-GET → `404`. Unauthenticated
  requests never reach the container. `/metrics` itself stays `404`.
- Rules live in `wrangler/src/alert-rules.ts`; each links to a
  [runbook](runbook.md#metrics-alerts) section (a test enforces the anchors):

| Rule | Fires when | Runbook |
| --- | --- | --- |
| `job_stale:<job>` | last success older than 2 x the job cadence (never-succeeded is ignored) | [job stale](runbook.md#alert-job-stale) |
| `job_consecutive_failures:<job>` | `two_bot_job_consecutive_failures` >= 3 | [job failures](runbook.md#alert-job-failures) |
| `rest_429_rate` | 429s > 10% of REST requests between samples, >= 10 requests | [REST 429](runbook.md#alert-rest-429) |
| `db_pool_saturated` | pool at max, 0 idle, 3 consecutive samples | [DB pool](runbook.md#alert-db-pool) |
| `db_errors` | 3+ storage failures between samples (restarts skip the window) | [DB errors](runbook.md#alert-db-errors) |
| `send_admission_blocked` | new admission refusals in 3 consecutive samples | [send admission blocked](runbook.md#alert-send-admission-blocked) |
| `voice_failures` | room-op failures > 5% of >= 10 ops between samples, or any new dead-letter/orphan (restarts skip the window) | [voice failures](runbook.md#alert-voice-failures) |

`job_stale` uses `JOB_INTERVAL_SECONDS`, which must equal each scheduled job's
Rust `*_INTERVAL_MS / 1000`. `invite_snapshot`, `session_checkpoint` and `other`
have no cadence and are exempt. `wrangler/test/alert-job-catalog.test.ts` fails
when a `JOBS` label has neither a matching cadence nor a reasoned exemption.

Packet identity (TOG-12100): rule ids above are the single shared spelling
used on both sides of the B2 soak evidence seam. The Rust canonical list is
`ALERT_RULE_IDS` in `crates/core/src/evidence.rs`; the Worker mirrors it in
`packetFilename` (`wrangler/src/alert-rules.ts`). Every evidence/alert packet
is named `evidence-{ruleId}-{window}.json` (soak-ledger packets stamp the
`soak_expected_committed` ledger identity), so the QA evidence table can
attribute packets when several rules fire in one window. Both sides pin all
seven spellings with tests; the payload shape is unchanged.

Known gaps: the DB error counter currently records only send-admission SQL,
so non-admission stores still surface only through the pool proxy and the
job-failure rules; adopt `Metrics::db_error` per store incrementally. A
slow trickle of DB errors below the burst threshold likewise surfaces only
through `job_consecutive_failures`.
A forced job failure on staging (three failures) raises
`job_consecutive_failures:<job>` within about one keepalive tick. Counter resets
(process restart) skip the 429 window. Alert state is persisted in DO storage
before notifying, so delivery is at most once per transition.
