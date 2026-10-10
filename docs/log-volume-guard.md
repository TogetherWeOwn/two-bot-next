# Log-volume and cardinality guard (gateway cutover path)

Offline guard over `docs/observability-event-catalog.md`. It sets per-event
rate caps and label-cardinality limits for the 48h watch, and names what
samples or drops first when the gateway path is overloaded. It changes no
emit site and installs no sampler: volume is controlled at subscription time
(intents), at the pipeline gates, and by parking jobs. The companion test
(`crates/bot/src/log_volume_guard_tests.rs`) pins every table below; a new
high-rate event without a cap row fails the suite.

## Why volume is bounded

- The per-dispatch hot path emits no log lines. `crates/bot/src/dispatch.rs`
  and `crates/bot/src/gateway_metrics.rs` contain no `tracing` use: the
  observer only bumps fixed-cardinality counters. A `tracing` addition in
  either file fails the guard test until it gets a cap row.
- Every metric label is a compile-time allowlist in
  `crates/core/src/metrics.rs`. Unknown events, routes, jobs and voice
  families collapse to `other`, which is itself a pinned series. No
  guild/member/channel ID, token, query string, body or message content ever
  becomes a label or a log field.

## Cardinality budget: 287 samples

`GET /metrics` renders this many non-comment samples from process start,
before any traffic. Adding any series fails the pinned count until this
table and the test are updated together.

| Family | Series | How |
| --- | --- | --- |
| `two_bot_gateway_events_total{event}` | 20 | `EVENTS` allowlist |
| reconnects, resumes, disconnects, missed | 4 | scalar counters |
| `two_bot_gateway_latency_seconds` | 1 | gauge, `NaN` until measured |
| `two_bot_handler_duration_seconds` | 11 | 8 buckets + `+Inf` + sum + count |
| `two_bot_rest_requests_total{route,result}` | 156 | 26 route templates x 6 results |
| job runs, timestamps, failure streaks | 48 | 12 jobs x (2 outcomes + timestamp + streak) |
| voice ops, reconcile, dead-letters, state | 31 | 3x5 ops + 4 reconcile + 9 dead-letter + tracked + compensation + orphans |
| pool gauges | 4 | configured, size, idle, max |
| `two_bot_db_errors_total{op}` | 2 | `admission`, `other` |
| `two_bot_send_admissions_total{outcome}` | 4 | `admitted`, `blocked`, `storage_error`, `other` |
| `two_bot_dispatch_drops_total{lane}` | 6 | `messages`, `interactions`, `registry`, `privileged`, `busy`, `reactions` |
| `# HELP` / `# TYPE` headers | 48 | 24 families x 2 |

## Per-event caps (gateway metric labels)

Class `session` and `steady` never shed: they are the watch signal.
Class `hot` sheds its downstream pipeline work first, in `shed` order, and
never gains a per-event log line. Budgets are steady-state expectations per
shard, not alert rules: exceeding them means investigate, using the
thresholds document for paging.

| Event label | Class | Steady-state budget | Shed |
| --- | --- | --- | --- |
| `READY` | session | ~0/hr; any burst is flapping | never |
| `RESUMED` | session | ~0/hr; pairs with disconnects | never |
| `GUILD_CREATE` | steady | rare (guild available) | never |
| `GUILD_DELETE` | steady | rare | never |
| `GUILD_UPDATE` | steady | rare | never |
| `GUILD_MEMBER_ADD` | steady | join rate of the guild | never |
| `GUILD_MEMBER_REMOVE` | steady | leave rate of the guild | never |
| `GUILD_MEMBER_UPDATE` | hot | unbounded; member churn | 7 |
| `MESSAGE_CREATE` | hot | unbounded; busiest dispatch | 1 |
| `MESSAGE_UPDATE` | hot | unbounded | 2 |
| `MESSAGE_DELETE` | hot | unbounded | 3 |
| `MESSAGE_REACTION_ADD` | hot | unbounded; self-role bursts on their own lane | 5 |
| `MESSAGE_REACTION_REMOVE` | hot | unbounded; self-role bursts on their own lane | 6 |
| `VOICE_STATE_UPDATE` | hot | unbounded; voice churn | 4 |
| `INVITE_CREATE` | steady | rare | never |
| `INVITE_DELETE` | steady | rare | never |
| `INTERACTION_CREATE` | steady | user-driven rate | never |
| `HEARTBEAT_ACK` | steady | ~1 per 45 s; the latency signal | never |
| `GATEWAY_CLOSE` | session | ~0/hr; pairs with reconnects | never |
| `other` | hot | collapsed unknowns; growth means a new Discord type arrived | 8, last: shedding the catchall blinds us |

## Log-line classes (catalog traced messages)

All 31 cataloged session-path messages plus the 2 drain outcomes are class
`session`: at most a handful per process lifetime, one per session or
failure. More than ~10 of any one line in 5 minutes means a flap or failure
storm, not traffic. They are never sampled. Per-command voice receipts and
startup diagnostics outside the catalog keep their owning docs; the guard
test presence-checks only the catalog rows.

## What sheds or drops first

Overload backpressure today fails over to restart, never to silent drop:
checkpoint IO times out after 5 s, dispatch IO after 20 s, drain after 30 s.
So shedding means doing less work per dispatch, in this order:

1. `presence_probe` job (hourly trend report; informational). Park first.
2. `community_scorecard`, then `inactivity` sweeps (1 min / hourly).
3. `audit_retry` (30 s) and `scheduled_messages` (15 s) tickers.
4. Message-content features: automod in-memory inspection and ticket body
   reads ride `MESSAGE_CREATE` only when the content flags justify the
   privileged intent; disabling them removes pipeline work, not gateway
   volume.
5. Voice reconcile detail (plan counts only; outcomes stay counted).
6. Never shed: session lifecycle lines, checkpoint commits, disconnect and
   missed-events counters, failure classes, or the `other` catchall.

The send-admission `blocked` counter is the pipeline's own shed meter:
refused admits are counted per bounded outcome, never silently dropped,
and storage failures land in `two_bot_db_errors_total` per bounded op.
Both counters are class steady and never shed.

Dispatch-lane saturation (TOG-19878) follows the same shed-meter shape: every
`spawn_first` refusal increments `two_bot_dispatch_drops_total{lane}` per
attempted lane, while the `warn!` samples the first drop per 60 s per runtime.
A burst is O(1) log lines with N counter increments. The six lane labels are
class steady and never shed; growth means a lane is undersized or a burst
needs the M2.1 alert rule, not a new label. The `reactions` lane additionally
counts per-member fairness refusals (member at `PER_USER_IN_FLIGHT` slots with
free lane slots), so `reactions` growth points at a hot member before an
undersized lane; the M2.1 alert rule should treat `reactions` drops as
member-hot until lane saturation is confirmed.

Subscription facts that bound the top of the funnel: the bot never requests
`GUILD_PRESENCES`, so presence arrives only through the hourly
`presence_probe` job, never as gateway events; `MESSAGE_CONTENT` is requested
only when automod, tickets or text commands justify it.

## Maintenance

The catalog, this guard, the conformance test and the guard test move
together. Add a dispatch label: add its cap row here and in `EVENT_CAPS`.
Add a job or voice family: same for `JOB_CAPS` and the cardinality table.
Add a dispatch lane: add its label row here and in `DISPATCH_LANE_CAPS`.
Add a log line on the session path: record it in the catalog and in
`SESSION_LOG_CAPS`. Unknowns fail closed on purpose.
