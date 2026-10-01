# Internal metrics

The Rust process exposes `GET /metrics` on its existing `LISTEN_ADDR` listener.
This is **internal-only**, unauthenticated operational data: scrape only from the
container/private network. Do not expose the container port publicly, add a Worker
proxy route, or route this through a public ingress. Both the Worker entrypoint
and the Container DO refuse `/metrics`; the Worker also reserves `/metrics`,
`/metrics/*` and canonical case/encoding/slash aliases before
invite-campaign lookup. No Prometheus server is added by this change.

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
| `two_bot_gateway_events_total{event}` | Received dispatches, including replays/duplicates, plus heartbeat ACKs and closes; fixed type allowlist, remainder `other` |
| `two_bot_handler_duration_seconds` | Cumulative histogram over nonduplicate dispatch parse/pipeline/durable commit, including failures; seconds |
| `two_bot_rest_requests_total{route,result}` | Executor HTTP sends, including retries; result `2xx`, `3xx`, `4xx`, `429`, `5xx` at response headers or `transport` (failure/cancellation/timeout before headers); later body failures do not hide 429/5xx |
| `two_bot_db_pool_configured` | Whether gateway initialization has registered a pool |
| `two_bot_db_pool_connections` | Current pool size |
| `two_bot_db_pool_idle_connections` | Current idle connections |
| `two_bot_db_pool_max_connections` | Configured maximum |
| `two_bot_job_runs_total{job,outcome}` | Completed attempts; outcome is `success` or `failure` (including returned errors, timeouts and isolated panics) |
| `two_bot_job_last_success_timestamp_seconds{job}` | Last successful completion time in Unix seconds; zero means no success recorded |
| `two_bot_job_consecutive_failures{job}` | Failed completions since the last success; resets to zero on success |

## Job coverage and outcomes

The supervisor records all three job metrics centrally after each completed
attempt. Individual periodic jobs need no instrumentation. The current scheduled
labels are `counter`, `rank`, `scheduled_events`, `presence_probe`,
`community_scorecard` and `inactivity` (the last two may be parked by configuration).
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
