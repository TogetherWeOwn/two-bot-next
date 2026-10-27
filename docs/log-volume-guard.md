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

## Cardinality budget: 787 samples

`GET /metrics` renders this many non-comment samples from process start,
before any traffic. Adding any series fails the pinned count until this
table and the test are updated together.

| Family | Series | How |
| --- | --- | --- |
| `two_bot_process_incarnation{part}` | 4 | process-lifetime identity as exact unsigned 32-bit numeric parts; only fixed labels `0`, `1`, `2`, `3`, never identity-valued labels |
| `two_bot_gateway_events_total{event}` | 21 | `EVENTS` allowlist |
| reconnects, resumes, disconnects, missed | 4 | scalar counters |
| `two_bot_gateway_rsvp_refused_total` | 1 | label-free counter: RSVP interactions refused past the 32 in-flight bound |
| `two_bot_gateway_latency_seconds` | 1 | gauge, `NaN` until measured |
| `two_bot_handler_duration_seconds` | 11 | 8 buckets + `+Inf` + sum + count |
| `two_bot_voice_join_to_move_seconds` | 11 | label-free; 8 buckets + `+Inf` + sum + count |
| `two_bot_rest_requests_total{route,result}` | 180 | 30 route templates x 6 results |
| `two_bot_rest_guard_*` | 11 | Fixed, label-free shared admission-guard counters and gauges |
| job runs, timestamps, failure streaks | 68 | 17 jobs x (2 outcomes + timestamp + streak) |
| `two_bot_job_last_error_class{job,class}` | 119 | 17 jobs x 7 classes (`database`, `rest`, `configuration`, `timeout`, `panic`, `feed`, `recovery_required`) |
| voice ops, reconcile, dead-letters, state | 60 | 3x6 ops + 4 reconcile + 9 dead-letter + tracked + compensation + orphans + 7 join-refused reasons + 2 queue lanes + halted + oldest-age + live-not-ready age + live self-heals + rest-age + creator-map generation + creator-map age + backoff-capped + dispatch-timeouts + watchdog-stucks + halt-recoveries + inbox-turn bound + inbox turns + dispatch turns + inbox-overflow drops + opt-in queue-overflow drops |
| `two_bot_voice_join_occupancy_total{occupancy}` | 8 | seeding-TEST occupancy buckets `0`-`6` + `7+`; fixed vocabulary, no per-channel/member labels |
| pool gauges | 4 | configured, size, idle, max |
| `two_bot_db_errors_total{op}` | 2 | `admission`, `other` |
| `two_bot_containment_store_errors_total{op}` | 2 | `complete_incident`, `other` |
| `two_bot_send_admissions_total{outcome}` | 4 | `admitted`, `blocked`, `storage_error`, `other` |
| `two_bot_gateway_prefix_trigger_refused_total{reason}` | 2 | `verdict`, `other` |
| `two_bot_dispatch_drops_total{lane}` | 6 | `messages`, `interactions`, `registry`, `privileged`, `busy`, `reactions` |
| `two_bot_raid_dropped_joins_total{reason}` | 2 | `queue_full`, `queue_closed`; fixed queue-drop reasons, no per-member/guild labels |
| `two_bot_join_risk_dropped_joins_total{reason}` | 2 | `queue_full`, `queue_closed`; fixed queue-drop reasons, no per-member/guild labels |
| `two_bot_voice_vote_kick_total{outcome}` | 26 | `started` + 2 worker refusals + 15 vote-core refusals (including `cooldown`, `initiator_limited`) + 7 enforcements + `other` |
| `two_bot_event_voice_tick_total{outcome}` | 6 | 5 website TBD reasons (`opens_soon`, `rate_limited`, `perm_denied`, `no_category`, `unavailable`) + `other`; one observation per TBD tick including backoff-skipped ticks |
| `two_bot_voice_names_total{outcome}` | 16 | 1 create + 5 template-render outcomes + 9 dispatch outcomes (including `name_blocked` for rename or custom-override refusal) + `other` |
| `two_bot_gateway_checkpoint_failures_total{stage}` | 2 | `pre_commit`, `commit` |
| `two_bot_drop_poison_total{guard}` | 7 | `command_dispatch_guard`, `schedule_run`, `user_slot`, `in_flight_guard`, `ticket_supervisor`, `response_accounting`, `voice_connection_guard`; fixed vocabulary, each recovery logs one warn |
| `two_bot_internal_actions_total{family,outcome}` | 72 | 6 families x 12 outcomes (`announcement`, `event`, `settings`, `moderation`, `membership`, `other` x `executed` + 11 refusal classes) |
| `two_bot_moderation_enforcements_total{verb,outcome}` | 45 | 9 verbs (`ban`, `tempban`, `kick`, `timeout`, `warn`, `purge`, `slowmode`, `lockdown`, `unlock`) x 5 outcomes (`success`, `refused`, `discord_error`, `storage_error`, `other`) |
| `two_bot_community_facts_drain_failures_total{reason}` | 3 | `error`, `timeout`, `other` |
| `two_bot_presence_capture_total{outcome}` | 3 | `inserted`, `duplicate`, `rejected` |
| `two_bot_automod_decisions_total{outcome}` | 8 | `accepted`, `already_processed`, `dry_run`, `protected`, `deleted`, `warned`, `timed_out`, `sanction_refused`; no message text, excerpts or user IDs in labels |
| `two_bot_audit_delivery_halt` | 1 | label-free gauge: 1 engaged, 0 cleared |
| `two_bot_event_store_consumer_lag_seconds` | 1 | label-free gauge: seconds of consumer lag, 0 when unobserved |
| `two_bot_eventsub_received_total` | 1 | label-free counter: deliveries accepted for verification, including tampered signatures |
| `two_bot_eventsub_classified_total{kind}` | 19 | `challenge`, `notification`, `revocation`, `tampered`, `ignored`, the thirteen `EventSubError` snake_case labels, `other`; unknown kinds collapse to `other` |
| `two_bot_eventsub_posts_total{outcome}` | 4 | `attempted`, `succeeded`, `failed`, `other`; zero until the go-live sender is wired |
| `two_bot_eventsub_reconcile_passes_total{outcome}` | 3 | `healthy`, `pending`, `failed`; one sample per pass, unknown outcomes collapse to `failed` |
| `two_bot_eventsub_reconcile_actions_total{action}` | 2 | `create`, `delete`; 409 already-exists confirmations fold into the pass outcome |
| `two_bot_eventsub_reconcile_errors_total{error_class}` | 5 | `reconcile_action_failed`, `reconcile_prior_failed`, `reconcile_unhealthy`, `reconcile_failed`, `other`; no status, body or response bytes in labels |
| `two_bot_feed_fetches_total{outcome}` | 4 | `success`, `invalid_source`, `refused_redirect`, `failure`; per-outcome, never per-feed |
| `two_bot_feed_fetch_errors_total{error_class}` | 12 | 11 named classes + `other` (`invalid_source`, `non_public_address`, `redirect_refused`, `too_large`, `invalid_encoding`, `unsupported_content_type`, `unsupported_encoding`, `resolve`, `transport`, `deadline`, `http_error`); successes record nothing here |
| `two_bot_retention_replay_needed_total` | 1 | label-free counter: skipped retention purges needing Operator replay (one per real guild removal whose purge-queue write hit the schema gap); resets on process restart |
| `two_bot_ticket_lifecycle_total{event,outcome}` | 22 | per-event subsets (2 `retire` + 3 `abandon` + 13 `purge` + 2 `claim` + 2 `close`): each event renders its success outcomes plus the `query` collapse trapdoor, and `purge` adds the 12 `db_cause` classes; closed vocabulary, no `other` row, no member text, channel names, ticket ids or error strings in labels |
| `two_bot_ticket_component_drops_total` | 1 | label-free counter: ticket components refused before spawning (runtime or per-member saturated); resets on process restart |
| `# HELP` / `# TYPE` headers | 172 | 86 families x 2 |

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
| `PRESENCE_UPDATE` | hot | unbounded; only with `TWO_VOICE_PRESENCE=1`; in-memory only, ordered, never checkpointed | 8 |
| `INVITE_CREATE` | steady | rare | never |
| `INVITE_DELETE` | steady | rare | never |
| `INTERACTION_CREATE` | steady | user-driven rate | never |
| `HEARTBEAT_ACK` | steady | ~1 per 45 s; the latency signal | never |
| `GATEWAY_CLOSE` | session | ~0/hr; pairs with reconnects | never |
| `other` | hot | collapsed unknowns; growth means a new Discord type arrived | 9, last: shedding the catchall blinds us |

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
   missed-events counters, failure classes, or the `other` catchall. `feeds`,
   `self_role_recovery`, `member_unban_sweep`, `automod_expiry` and `lfg_sweep` completion
   metrics also never shed; park these jobs only through their existing
   feature configuration, not the log guard.

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

Website-action receiver executions (TOG-20119, roadmap M4.23) follow the same
shape: every signed-receiver request increments
`two_bot_internal_actions_total{family,outcome}` exactly once, while refusal
`warn!` summaries stay sampled. A burst is O(1) log lines with N counter
increments. The six families and twelve outcomes are class steady and never
shed; unknown verbs collapse to `other` and unknown outcomes to `internal`,
so growth means a real family or refusal class arrived and needs the M2.1
alert rule, not a new label.

Fatal-runner reasons are class `session` and O(1) bytes: both dispatch-join
`map_err` sites in `crates/bot/src/gateway.rs` pass the surfaced reason
through `bounded_runner_reason`, capped at `RUNNER_REASON_MAX_CHARS`
(512 chars, char-boundary). Today every reason is one of six `&'static str`
literals from `dispatch_bounded` (the `JoinError` payload is discarded
there), so the type already bounds the output; the cap is a fence that holds
even if a future supervisor returns a larger payload.

Subscription facts that bound the top of the funnel: the bot requests
`GUILD_PRESENCES` only with `TWO_VOICE=1` and `TWO_VOICE_PRESENCE=1`
(`docs/voice-presence.md`). Those `PRESENCE_UPDATE` dispatches stay in
gateway order on the dispatch worker but only update in-memory voice-room
facts: no funnel, audit or checkpoint commit. Without the flag, presence
arrives only through the hourly `presence_probe` job. `MESSAGE_CONTENT` is requested only when
automod, tickets or text commands justify it.

## Maintenance

The catalog, this guard, the conformance test and the guard test move
together. Add a dispatch label: add its cap row here and in `EVENT_CAPS`.
Add a job or voice family: same for `JOB_CAPS` and the cardinality table.
Add a dispatch lane: add its label row here and in `DISPATCH_LANE_CAPS`.
Add a receiver family or outcome: add its label row here and in
`INTERNAL_ACTION_CAP_FAMILIES` / `INTERNAL_ACTION_CAP_OUTCOMES`.
Add a log line on the session path: record it in the catalog and in
`SESSION_LOG_CAPS`. Unknowns fail closed on purpose.
