# Observability event catalog (gateway/session path)

Read-only extract of the structured log event names and metric labels
emitted on the gateway cutover path and the scheduled-job / alert-rule
path. This file renames nothing and changes
no emit site; it records the current spellings so watch queries, scrapers
and alert rules can match them exactly.

Method: offline code search over `crates/bot/src/gateway.rs`,
`crates/bot/src/main.rs`, `crates/bot/src/server.rs`,
`crates/bot/src/shutdown.rs`, `crates/bot/src/gateway_metrics.rs`,
`crates/bot/src/gateway_failure.rs`, `crates/core/src/metrics.rs` and
`crates/bot/src/voice_rooms.rs`, plus the scheduled-job producers
(`feed_jobs.rs`, `self_role_handlers.rs`, `member_runtime.rs`,
`automod_gateway.rs`), the job allowlist and render in `metrics.rs`
(`JOBS`, `render_with_rest_guard`), and the Worker alert sources
(`metrics-scrape.ts` `ALERT_INPUT_LABELS`, `alert-rules.ts` `RULES` and
`evaluateMetrics`). Rust callsites name the file and the enclosing
item (function or method), never a line number: line numbers drift as the
code moves, so re-run the search before quoting a line in an incident.
Worker rule rows name the file and function for the same reason.

## Gateway lifecycle log lines

The message string (last argument to the `tracing` macro) is the event
name. Fields such as `sequence`, `resume`, `error`, `job_id` or
`guild_id` are context, not the name.

| Event name | Callsite (stable anchor) | Meaning |
| --- | --- | --- |
| `gateway intents resolved` | `crates/bot/src/main.rs` (`run`) | Boot resolved the gateway intents; `voice_presences` states whether `GUILD_PRESENCES` was requested (the enabling flag may be a secret binding, so only the boolean is logged). `false` means voice-room game names can never be detected this boot |
| `durable gateway initialized; shard connecting` | `crates/bot/src/main.rs` (`run`) | Supervisor built the shard; `resume=true` means a saved checkpoint was offered, `false` means fresh IDENTIFY |
| `gateway shard loop started` | `crates/bot/src/gateway.rs` (`run_shard`) | Reception task entered the Twilight stream loop |
| `gateway reconnect failed; Twilight will retry` | `crates/bot/src/gateway.rs` (`run_shard`) | Transport reconnect attempt failed; Twilight owns the retry |
| `cold resume committed; requesting voice snapshot via identify` | `crates/bot/src/gateway.rs` (`run_shard`) | Cold voice RESUME committed; reception IDENTIFies for a fresh voice snapshot |
| `gateway ready; checkpoint committed` | `crates/bot/src/gateway.rs` (`apply_dispatch`) | Dispatch committed and shard marked Connected |
| `gateway leveling dispatch failed` | `crates/bot/src/gateway.rs` (`apply_dispatch`) | Leveling funnel drain failed inside the checkpoint deadline |
| `gateway community facts dispatch failed` | `crates/bot/src/gateway.rs` (`apply_dispatch`) | Community facts drain failed; worker continues and the scorecard fails closed on missing coverage |
| `gateway community facts dispatch timed out` | `crates/bot/src/gateway.rs` (`apply_dispatch`) | Community facts drain missed the checkpoint deadline; worker continues and the scorecard fails closed on missing coverage |
| `gateway onboarding job invalid` | `crates/bot/src/gateway.rs` (`apply_dispatch`) | Dispatch-worker durable onboarding payload failed to serialize; recorded as a typed error, checkpoint unchanged |
| `onboarding interaction interrupted; member must reselect` | `crates/bot/src/gateway.rs` (`onboarding_queue`) | Durable onboarding job recovered without callback credentials; kept as interruption receipt |
| `invite counter read unavailable; retaining snapshot` | `crates/bot/src/gateway.rs` (`HttpInvites::current`) | REST invite read failed; persisted baseline kept instead of an empty listing |
| `interaction acknowledgement blocked; advancing past lost callback` | `crates/bot/src/gateway.rs` (`complete_acknowledgement`) | Ordered RSVP acknowledgement hit send-admission Blocked; checkpoint advances past the lost callback without replaying uncertain effects |
| `interaction response failed; not replaying command` | `crates/bot/src/gateway.rs` (`complete_acknowledgement`, three sites) | Ordered RSVP preparation or completion failed without admission blockage; command advances without replaying uncertain effects |
| `READY identity differs from boot token; ordered identity not armed` | `crates/bot/src/gateway.rs` (`run_shard`) | READY application id differs from the boot/REST pin; ordered identity stays disarmed and its fence keeps refusing |
| `retention lifecycle skipped; schema gap, nothing to cancel` | `crates/bot/src/gateway.rs` (`retention_lifecycle`) | Rejoin-cancel hit a schema gap (missing table/permission, SQLSTATE only); skipped, nothing to cancel, gateway survives a database behind the binary |
| `retention purge skipped; schema gap, operator replay required` | `crates/bot/src/gateway.rs` (`retention_lifecycle`) | Real guild removal hit a schema gap; gateway acked past the only purge-queue write, Operator must replay it per privacy.md once the migration lands |
| `gateway prerequisites missing; gateway parked, /readyz reports down` | `crates/bot/src/main.rs` (`run`) | Token, database URL or guild ID missing; shard never starts |
| `gateway readiness failure injection active; /readyz reports gateway down for the staging rollback drill` | `crates/bot/src/main.rs` (`run`) | Staging-fenced rollback-drill injection is on; `/readyz` reports the gateway component down so the staging gate fails and rolls back. The fence refuses outside staging, so this line never appears in production |
| `feature gates invalid; ordered interaction surface parked` | `crates/bot/src/main.rs` (`build_interaction_runtime`) | Feature-gate parsing failed; gateway still boots, only the ordered interaction surface stays off |
| `moderation gates invalid; ordered interaction surface parked` | `crates/bot/src/main.rs` (`build_interaction_runtime`) | Moderation-gate parsing failed; gateway still boots, only the ordered interaction surface stays off |
| `durable gateway failed; checkpoint unchanged, readiness unavailable` | `crates/bot/src/main.rs` (`publish_gateway_failure`) | Gateway task failed with the fixed class in `error_class`; checkpoint not advanced |
| `gateway task stopped; container restart required` | `crates/bot/src/main.rs` (`supervise_gateway_bounded`) | Supervisor saw the essential task end; process must restart from checkpoint |
| `gateway drain failed; restart required` | `crates/bot/src/main.rs` (`supervise_gateway_bounded`) | Drain path failed; restart required |
| `container service failed` | `crates/bot/src/main.rs` (`run`) | Service supervisor failed (`startup_phase=service_supervisor`) |
| `shutdown_deadline_exceeded: abandoning in-flight work` | `crates/bot/src/main.rs` (`supervise_gateway_bounded`) | Drain deadline elapsed; in-flight work abandoned |
| `listening` | `crates/bot/src/server.rs` (`bind`) | HTTP listener bound; one line per process start |
| `SIGTERM received; draining` | `crates/bot/src/server.rs` (`shutdown_signal`) | SIGTERM observed; effects drain before exit |
| `SIGINT received; draining` | `crates/bot/src/server.rs` (`shutdown_signal`) | SIGINT observed; effects drain before exit |
| `shutdown_completed` | `crates/bot/src/server.rs` (`serve_with_shutdown`) | HTTP graceful drain completed; stable `msg` name with no human message |
| `response failed` | `crates/bot/src/server.rs` (`router_with_guard` trace layer, two sites) | HTTP 5xx response from the trace layer; DEBUG for the routine `/readyz` 503, ERROR otherwise |
| `second shutdown signal received; exiting immediately` | `crates/bot/src/shutdown.rs` (`exit_on_second_signal`) | Second signal during drain; process exits at once |
| `invalid shutdown timeout; using default` | `crates/bot/src/shutdown.rs` (`deadline_from`) | `SHUTDOWN_TIMEOUT_SECONDS` unparsable; default deadline kept |
| `periodic job failed` | `crates/bot/src/jobs.rs` (`record_completion`) | Completed scheduled-job attempt failed; `job` and `error_class` name the job |
| `TWO_VOICE=1 but no discord token; voice rooms disabled` | `crates/bot/src/gateway.rs` (`build_voice_runtime`) | Voice gate on but no token; gateway continues voice-off |
| `TWO_VOICE=1 but no database URL; voice rooms disabled` | `crates/bot/src/gateway.rs` (`build_voice_runtime`) | Voice gate on but no database; gateway continues voice-off |
| `voice database unavailable; voice rooms disabled` | `crates/bot/src/gateway.rs` (`build_voice_runtime`) | Voice store connect failed; gateway continues voice-off |
| `voice rooms enabled; gateway sink attached` | `crates/bot/src/gateway.rs` (`build_voice_runtime`) | Voice runtime built; sink attached to the gateway writer |
| `voice HTTP setup failed; voice rooms disabled` | `crates/bot/src/gateway.rs` (`build_voice_runtime`) | Voice HTTP setup failed; gateway continues voice-off |
| `eventsub startup refused` | `crates/bot/src/main.rs` (`run`, two sites) | EventSub receiver config invalid or bind failed; process exits for supervisor restart |
| `eventsub delivery refused` | `crates/bot/src/eventsub_http.rs` (`refuse`) | Delivery failed verification, admission, shape or classification; `status` and `error_class` name the refusal, bounded `message_type` (`webhook_callback_verification` / `notification` / `revocation` / `unknown`) names the delivery type (`unknown` before headers are established) |
| `challenge answered` | `crates/bot/src/eventsub_http.rs` (`receive`) | Webhook verification challenge answered with the plaintext challenge; Twitch enables the subscription |
| `notification acknowledged` | `crates/bot/src/eventsub_http.rs` (`handle_notification`, two sites) | Notification verified and acknowledged; when delivery dependencies are injected, the durable claim → send → settle flow has settled (or deduped) before the ack |
| `revocation acknowledged` | `crates/bot/src/eventsub_http.rs` (`receive`) | Revocation verified and acknowledged; boot/periodic reconcile owns re-subscription; bounded `reason` (`user_removed` / `authorization_revoked` / `notification_failures_exceeded` / `version_removed` / `other`) answers counts by reason |
| `delivery ignored` | `crates/bot/src/eventsub_http.rs` (`receive`, two info sites) | Known ignored delivery or deterministic planning error acknowledged with 2xx; unknown/future classification errors are counted as `other` and refused with retryable 503 |
| `event = "eventsub_receiver"` | `crates/bot/src/eventsub_http.rs` (`refuse`, `receive`, `handle_notification`) | Bounded `event` field value every receiver emission carries; no header, token or body leaves the process |
| `eventsub reconcile parked: TWO_EVENTSUB is not exactly 1` | `crates/bot/src/eventsub_reconcile.rs` (`startup_reconcile`, `spawn_if_enabled`) | Info; gate dark, no subscription is listed or mutated (`status = "parked"`, `reason = "flag_dark"`) |
| `eventsub reconcile refused: callback configuration is invalid` | `crates/bot/src/eventsub_reconcile.rs` (`startup_reconcile`) | Error; `TWO_TWITCH_CALLBACK_URL` missing or not an https callback, no Helix call (`error_class = "reconcile_config_invalid"`) |
| `eventsub reconcile parked: broadcaster allowlist is empty, no subscriptions mutated` | `crates/bot/src/eventsub_reconcile.rs` (`startup_reconcile`) | Warn; `TWO_TWITCH_BROADCASTERS` empty, pass parked without mutation (`error_class = "reconcile_no_broadcasters"`) |
| `eventsub reconcile refused: Twitch credentials are missing or invalid` | `crates/bot/src/eventsub_reconcile.rs` (`startup_reconcile`) | Error; Operator Twitch client credentials unreadable, no Helix call (`error_class = "reconcile_config_invalid"`) |
| `eventsub reconcile refused: webhook secret is missing or invalid` | `crates/bot/src/eventsub_reconcile.rs` (`startup_reconcile`) | Error; signing secret unreadable, no Helix call (`error_class = "reconcile_config_invalid"`) |
| `eventsub reconcile found failed subscriptions on our callback; deleting and re-creating` | `crates/bot/src/eventsub_reconcile.rs` (`reconcile_all_with_metrics`) | Error; failed subscriptions on our callback counted before the pass (`error_class = "reconcile_prior_failed"`), then deleted and re-created |
| `eventsub subscriptions re-created but not all verified yet` | `crates/bot/src/eventsub_reconcile.rs` (`reconcile_all_with_metrics`) | Warn; pass re-created subscriptions but at least one desired broadcaster is not `enabled` yet (`status = "pending"`, `error_class = "reconcile_pending"`) |
| `eventsub subscriptions reconciled` | `crates/bot/src/eventsub_reconcile.rs` (`reconcile_all_with_metrics`) | Info; every desired broadcaster holds an `enabled` subscription on our callback (`status = "healthy"`) |
| `eventsub reconcile action failed; continuing with the rest of the plan` | `crates/bot/src/eventsub_reconcile.rs` (`run_plan`) | Error; one delete or create was rejected or failed in transport (`action = "delete"` or `"create"`, `error` names the status, `error_class = "reconcile_action_failed"`); the later actions and the follow-up listing still run |
| `eventsub reconcile did not converge: an action failed or a healthy subscription is missing` | `crates/bot/src/eventsub_reconcile.rs` (`reconcile_all_with_metrics`) | Error; after the follow-up listing, at least one action failed or a desired broadcaster lacks a healthy subscription; the pass fails loudly with `failed` and `missing` counts (`error_class = "reconcile_unhealthy"`) |
| `eventsub reconcile failed` | `crates/bot/src/eventsub_reconcile.rs` (`startup_reconcile`) | Error; the pass returned an error after the gate and config checks (`error_class = "reconcile_failed"`) |
| `event = "eventsub_reconcile"` | `crates/bot/src/eventsub_reconcile.rs` (`reconcile_all_with_metrics`, `run_plan`, `startup_reconcile`, `spawn_if_enabled`) | Bounded `event` field value every reconcile emission carries; scalar counts, statuses and error classes only, never a secret, header, challenge or body |

## Gateway metric event labels

Fixed allowlist `metrics::EVENTS` in `crates/core/src/metrics.rs`, observed in
`Observer::observe_text` in `crates/bot/src/gateway_metrics.rs`. Unknown
dispatch types collapse to `other`; scrapers must match these exact spellings.

| Event label | Series | Meaning |
| --- | --- | --- |
| `READY` | `two_bot_gateway_events_total{event="READY"}` | Fresh session (IDENTIFY accepted) |
| `RESUMED` | `two_bot_gateway_events_total{event="RESUMED"}` plus `two_bot_gateway_resumes_total` | Continued session (RESUME accepted) |
| `GUILD_CREATE` | `two_bot_gateway_events_total` | Guild available dispatch |
| `GUILD_DELETE` | `two_bot_gateway_events_total` | Guild unavailable/removed dispatch |
| `GUILD_UPDATE` | `two_bot_gateway_events_total` | Guild updated dispatch |
| `GUILD_MEMBER_ADD` | `two_bot_gateway_events_total` | Member joined dispatch |
| `GUILD_MEMBER_REMOVE` | `two_bot_gateway_events_total` | Member left dispatch |
| `GUILD_MEMBER_UPDATE` | `two_bot_gateway_events_total` | Member updated dispatch |
| `MESSAGE_CREATE` | `two_bot_gateway_events_total` | Message created dispatch |
| `MESSAGE_UPDATE` | `two_bot_gateway_events_total` | Message edited dispatch |
| `MESSAGE_DELETE` | `two_bot_gateway_events_total` | Message deleted dispatch |
| `MESSAGE_REACTION_ADD` | `two_bot_gateway_events_total` | Reaction added dispatch (self-role lane) |
| `MESSAGE_REACTION_REMOVE` | `two_bot_gateway_events_total` | Reaction removed dispatch (self-role lane) |
| `VOICE_STATE_UPDATE` | `two_bot_gateway_events_total` | Voice state changed dispatch |
| `PRESENCE_UPDATE` | `two_bot_gateway_events_total` | Member presence changed (only with `TWO_VOICE_PRESENCE=1`) |
| `INVITE_CREATE` | `two_bot_gateway_events_total` | Invite created dispatch |
| `INVITE_DELETE` | `two_bot_gateway_events_total` | Invite deleted dispatch |
| `INTERACTION_CREATE` | `two_bot_gateway_events_total` | Interaction received dispatch |
| `HEARTBEAT_ACK` | `two_bot_gateway_events_total` | Heartbeat acknowledged (opcode 11) |
| `GATEWAY_CLOSE` | `two_bot_gateway_events_total` | Close frame observed at reception |
| `other` | `two_bot_gateway_events_total{event="other"}` | Any dispatch type outside the allowlist |

Scalar gateway series pair with the labels above:

| Series | Meaning |
| --- | --- |
| `two_bot_gateway_reconnects_total` | New HELLOs after the first in the running loop |
| `two_bot_gateway_resumes_total` | Received RESUMED dispatches |
| `two_bot_gateway_disconnects_total` | Transport losses through the shard supervisor; each must pair with a later RESUME or fresh READY |
| `two_bot_gateway_missed_events_total` | Sequence gaps inside one session; any nonzero increase fails the zero-missed-events acceptance |
| `two_bot_gateway_latency_seconds` | Last heartbeat ACK round-trip; `NaN` until measured |
| `two_bot_gateway_prefix_trigger_refused_total{reason="verdict"}` | In-scope prefix candidates refused by the automod verdict; the worker records candidates even when its capture-only gate prevents detached dispatch. Unmatched content, scope mismatches, bot/webhook messages and disabled text-command gates never increment it |
| `two_bot_presence_capture_total{outcome}` | Presence-update capture decisions on the hot path; `outcome` is `inserted` (first delivery stored a row), `duplicate` (same-key retry absorbed by the dedup key) or `rejected` (refused before the insert) |
| `two_bot_automod_decisions_total{outcome}` | Terminal automod inspection outcomes; `outcome` is `accepted`, `already_processed`, `dry_run`, `protected`, `deleted`, `warned`, `timed_out` or `sanction_refused`. Recorded once per committed `StoredOutcome` by the shared activation; replays and retained claims never count. Fixed eight series from process start; no message text, excerpts or user IDs become labels |

## Voice stall gauge series

Aggregated across all current guild actors. Before the aggregation change a
single guild actor's publish overwrote the process values, so a healthy
guild could hide another guild's halt between scrapes; now each actor keeps
its latest contribution in a process-local map keyed by an opaque actor key
under an incarnation generation, and exposition renders aggregates under
the existing series names.

Sources, by stable anchor rather than line number: per-actor contributions
are recorded by `set_voice_actor_halted`, `set_voice_actor_queue` and
`remove_voice_actor` in `crates/core/src/metrics.rs` and rendered by
`render` in the same file; the guild actor publishes its incarnation's
contribution every tick from `publish_voice_gauges` in
`crates/bot/src/voice_rooms.rs` and retires it on loop exit through the
actor metric guard's `Drop` in the same file, so a late publish from a
replaced incarnation cannot resurrect it. Full semantics live in
`docs/metrics.md`.

The received-vs-handled drift signal stays with the `voice_join_received`
row in the voice structured-fields section below and is not duplicated
here: that row counts every delivery into the guild actor, while these
gauges show the actor-side stall posture (halted, queued depth, oldest
wait). Bounded cardinality holds: the only label is the fixed
`lane="urgent" | "deferred"` pair on the queue-depth family (two series,
never per-actor/per-guild/per-channel/per-member); the halt and oldest-age
gauges are label-free. Queue sums saturate instead of wrapping, and no
actor, guild, channel, member, token or body content ever reaches a label.

| Series | Labels | Scrape inputs | Alert usage | Meaning |
| --- | --- | --- | --- | --- |
| `two_bot_voice_worker_halted` | None (label-free gauge) | Additional series: accepted but not required by the scrape validator, which pins only the alert-input label sets; read manually from the container exposition or the authenticated pull | Consumed by no alert rule; manual check named in the runbook fast-burn, slow-burn and creator-map-stale first responses | 1 while any current guild actor is halted by a store CredentialRefused or Discord 401, otherwise 0; a healthy guild cannot hide another guild's halt |
| `two_bot_voice_queue_pending` | Fixed `lane`: `urgent` or `deferred` (exactly two series) | Same additional-series handling as above | Consumed by no alert rule; manual queue-side complement to the join-budget burn rules | Queued (not in-flight) voice actions per lane, saturating-summed across all current guild actors |
| `two_bot_voice_queue_oldest_age_seconds` | None (label-free gauge) | Same additional-series handling as above | Consumed by no alert rule; manual queue-side complement to the join-budget burn rules | Age in seconds of the oldest queued voice action, the maximum across all current guild actors; 0 when every queue is empty |

## EventSub delivery series

Counters emitted by the Twitch EventSub receiver render. Unknown `kind`
and `outcome` values collapse to `other`, and no member ID, token, header,
body, broadcaster, subscription or secret data reaches any label.

| Series | Meaning |
| --- | --- |
| `two_bot_eventsub_received_total` | Verified-shape EventSub deliveries accepted for signature verification, including tampered signatures; requests rejected before body intake are not deliveries |
| `two_bot_eventsub_classified_total{kind}` | Deliveries by bounded receiver classification; `kind` is `challenge`, `notification`, `revocation`, `tampered`, `ignored` or `other`; `other` marks unknown/future classification errors returned as retryable 503 |
| `two_bot_eventsub_posts_total{outcome}` | Go-live post attempts by bounded outcome; `outcome` is `attempted`, `succeeded`, `failed` or `other`; zero until the go-live sender slice is wired |
| `two_bot_eventsub_reconcile_passes_total{outcome}` | Reconcile passes by bounded outcome; `outcome` is `healthy`, `pending` or `failed`; exactly one sample per pass that reaches Helix, parked or refused startup passes stay log-only |
| `two_bot_eventsub_reconcile_actions_total{action}` | Successful reconcile mutations by bounded action; `action` is `create` or `delete`; 409 already-exists confirmations fold into the pass outcome |
| `two_bot_eventsub_reconcile_errors_total{error_class}` | Reconcile failure sites by bounded log `error_class`; `error_class` is `reconcile_action_failed`, `reconcile_prior_failed`, `reconcile_unhealthy`, `reconcile_failed` or `other`; no status, body or response bytes in labels |

## Scheduled job labels

Each label owns independent `two_bot_job_runs_total{job,outcome}`,
`two_bot_job_last_success_timestamp_seconds{job}`,
`two_bot_job_consecutive_failures{job}` and
`two_bot_job_last_error_class{job,class}` series (see the job allowlist
`JOBS` in `metrics.rs`). Success in one label never clears another label's
streak. None of these four has a `JOB_INTERVAL_SECONDS` cadence entry, so
staleness tickets via `job_unknown_stale` below instead of `job_stale`.

| Job label | Producer (stable anchor) | Meaning |
| --- | --- | --- |
| `feeds` | `feed_jobs.rs` (`scheduled_job_live` via `register` / `register_fenced`) | Feed poller sweep on the boot-configured interval with a live inner polling gate; gated skips return success and reset the streak |
| `self_role_recovery` | `self_role_handlers.rs` (`recovery_job`) | Self-role repair sweep every 30 s; a successful sweep need not settle every pending repair; failure-only alerting, no `job_stale` paging |
| `member_unban_sweep` | `member_runtime.rs` (`sweep_job`) | Member unban sweep every 30 s with an attempt timeout that may outlive its cadence; a successful sweep need not complete any unban; failure-only alerting |
| `automod_expiry` | `automod_gateway.rs` (`expiry_job`) | Automod repeat-history expiry tick every 60 s; a gateway without built activation is a no-op success, and real expiry work runs only with the automod gate on; failure-only alerting |

## Alert rules

Rule ids evaluated in `alert-rules.ts` (`evaluateMetrics`) against the
scrape inputs pinned in `metrics-scrape.ts` (`ALERT_INPUT_LABELS`). The id
is the single shared spelling also pinned in `evidence.rs`
(`ALERT_RULE_IDS`), so soak evidence packets attribute correctly.

| Rule | Input series | Meaning |
| --- | --- | --- |
| `rest_5xx_rate` | `two_bot_rest_requests_total{result="5xx"}` and all `two_bot_rest_requests_total` samples | Pages when at least 10 new REST requests arrive between samples and more than 10% are 5xx; a counter reset suppresses the window. |
| `job_unknown_stale` | `two_bot_job_last_success_timestamp_seconds{job}` for labels outside `JOB_INTERVAL_SECONDS`, `TICKER_STALE_JOBS` and `UNKNOWN_STALE_EXCLUDED` | Ticket when a job without a cadence entry has a nonzero success timestamp older than 2 hours; never-succeeded stays silent; `session_checkpoint` is event-driven and excluded |
| `gateway_unpaired_disconnect` | `two_bot_gateway_disconnects_total` against `two_bot_gateway_resumes_total` plus fresh `two_bot_gateway_events_total{event="READY"}` | Pages after 3 consecutive samples with disconnect growth and no RESUMED or fresh READY growth (pairing in `pairGatewayDisconnects`); any recovery growth clears at once; the disconnect counter increments in `gateway.rs` (`transport_disconnected`) |
| `gateway_not_ready` | `/readyz` `components` gateway entry (parsed by the keepalive probe, not a `/metrics` series) with the pairing counters as the restart signal | Pages after 3 consecutive valid samples with the gateway component `starting`/`down` and no disconnect edge required (stuck starting, parked, or draining); any ready sample clears at once; invalid probes and failed scrapes hold the previous state |
| `voice_join_fast_burn` | `two_bot_voice_joins_received_total` minus benign `duplicate` refusals in `two_bot_voice_join_refused_total` against `two_bot_voice_join_to_move_seconds_bucket{le="5"}` | Pages when the creator-join budget (99.5% moved within 5 s) burns faster than 14.4x in both the 1 h and 5 min windows; every non-duplicate refusal, failed move and slow (>5 s) move is burn; each window needs at least 10 genuine joins and two bad moves, receive counts lag one scrape, and incomplete post-restart windows stay inactive; missing inputs hold the previous state |
| `voice_join_slow_burn` | Same inputs as `voice_join_fast_burn` | Ticket, not page: same budget burning faster than 6x in both the 6 h and 30 min windows with the same per-window volume and bad-move floors; catches the sustained grind the fast rule never spikes on; do not close as "fast burn resolved" |
| `voice_creator_map_stale` | `two_bot_voice_creator_map_age_seconds` with `two_bot_voice_creator_map_generation` | Pages when the age since the last successful creator-map merge passes 600 s (two missed 5 min refreshes) after at least one merge; generation 0 (never merged: boot or voice parked) never fires; the gauges are optional scrape inputs, so older containers hold the previous state |
| `eventsub_post_failed_share` | `two_bot_eventsub_posts_total` by fixed `outcome` (`attempted`, `succeeded`, `failed`, `other`) | Ticket, not page: failed posts exceed 10% of attempted posts between samples with at least 10 attempted; the first valid sample is baseline only, missing/invalid samples preserve, any backwards counter rebaselines, and flat/sub-floor windows resolve |

## Gateway failure classes (`/readyz` `gateway_failure`)

`FailureClass` enum in `crates/bot/src/gateway_failure.rs`
(`FailureClass::ALL` / `as_str`), served as
`{"phase":"durable_gateway","class":"…"}`. One class is logged once by
`publish_gateway_failure`; SQL errors never reach the field.

| Class | Meaning |
| --- | --- |
| `store_unavailable` | Shared store not handed to the gateway task |
| `gateway_pool_connect_failed` | Dedicated checkpoint pool failed to open |
| `checkpoint_load_failed` | Durable checkpoint read failed |
| `onboarding_gates_invalid` | Onboarding gate parsing failed |
| `onboarding_init_failed` | Onboarding runtime initialization failed |
| `custom_commands_init_failed` | Command registry bootstrap failed |
| `milestones_load_failed` | Persisted leveling milestones read failed |
| `automod_config_invalid` | Automod environment rejected |
| `automod_executor_failed` | Automod REST executor build failed |
| `raid_executor_failed` | Raid-watch REST executor build failed |
| `gateway_runtime_failed` | Failure after the shard started running |
| `gateway_task_panicked` | Gateway task panicked |

Startup `error_class` values logged before fatal exits (see
`docs/startup-diagnostics.md`): `listener_bind_failed`,
`gateway_override_invalid`, `database_connect_failed`,
`receiver_config_invalid`, `receiver_prerequisites_invalid`,
`receiver_bind_failed`, `moderation_disable_unknown`,
`container_service_failed`, plus the twelve classes above.

## Voice structured fields (gateway sink, adjacent)

`voice_event` fields in `crates/bot/src/voice_rooms.rs`; no channel,
member, token, body or ID leaves the process. Included because the voice
sink attaches to the gateway writer on the cutover path.

| Field value | Callsite (stable anchor) | Meaning |
| --- | --- | --- |
| `voice_event="voice_join"` | `crates/bot/src/voice_rooms.rs` (`JoinTiming::finish`: `voice join completed`) | One terminal accepted creator join; bounded `outcome` and seconds for gateway/actor `queue_dwell`, `wait_before_dispatch`, `claim`, `reorder`, `post`, `persist`, `move`, and total `elapsed`. Stage times accumulate retries; the label-free `two_bot_voice_join_to_move_seconds` histogram samples confirmed successful moves only. |
| `voice_event="voice_operation"` | `crates/bot/src/voice_rooms.rs` (`observe_voice_operation`: `voice_operation succeeded` / `voice_operation failed`) | One finished room create/move/delete outcome with bounded `op`/`outcome` |
| `voice_event="voice_respace"` | `crates/bot/src/voice_rooms.rs` (`observe_voice_respace`: `voice respace succeeded` / `voice respace failed`) | One finished post-move respace repair with bounded `outcome`; cosmetic, never increments a lifecycle counter |
| `voice_event="voice_reconcile"` | `crates/bot/src/voice_rooms.rs` (`reconcile`: `voice_reconcile planned`) | One reconcile pass plan size with per-action counts |
| `voice_event="voice_dead_letter"` | `crates/bot/src/voice_rooms.rs` (`mark_failed_observed`: `voice action dead-lettered`) | Queue write exhausted retries with bounded `action` and attempts |
| `voice_event="voice_creator_orphan"` | `crates/bot/src/voice_rooms.rs` (`execute_create`: `voice creator orphan needs manual deletion`) | Untracked creator-channel orphan needing manual deletion |
| `voice_event="voice_join_received"` | `crates/bot/src/voice_rooms.rs` (`accept_join_received`: `voice creator join accepted` / `voice creator join refused before queueing`) | One creator-channel join delivered to the guild actor, accepted or refused. The refused line carries a bounded `reason` (`halted`, `access`, `unknown_creator`, `stale_ticket`, `duplicate`, `name_blocked`, `other`) plus the refusing `channel` id and creator-map state at join time (`creators_len`, `known`, `halted`, `ready`, `live_generation`, `ticket_generation`; no member/name/token content); every delivery bumps `two_bot_voice_joins_received_total`, so received-vs-handled drift is the stall signal |

Rename-decision lines without a `voice_event` field:
`voice room rename decision` (`log_rename_decision` and
`log_template_name_decision` in `crates/bot/src/voice_rooms.rs`): one line
per voice-room rename decision so a room that keeps its default name is
diagnosable from logs alone. Fields: `guild`, `channel` and bounded
`decision` (`queued`, `unchanged`, `channel_unseen`, `template_blocked`,
`attempted`, `renamed`, `rate_limited`, `permission_denied`,
`duplicate_name`, `in_flight_deferred`, `outcome_deferred`,
`outcome_unknown`, `discord_error`, or a `rename_*` skip reason). No
member-derived text (display names, stream or presence titles, rendered
room names) is logged. The execution lines also carry `reason` (empty
when none) and `retry_after_ms` (0 unless Discord asked for a wait);
failures log at warn. The template lines instead carry `playing_members`
and `activity_detected`, so "no game was ever seen" is distinguishable
from "the rename was refused"; the retry-only `channel_unseen` decision
logs at debug, since the naming pass retries it on every pass.

Adjacent actor message without a `voice_event` field:
`voice actor load failed; actor respawns on the next guild snapshot`
(`observe_voice_actor_load_failure` in `crates/bot/src/voice_rooms.rs`):
the guild actor's store load failed. Every failure bumps
`two_bot_db_errors_total{op="other"}`; the warn is throttled process-wide to
one line per 5 minutes, so a burst across guilds names only the first. The
actor respawns on the next guild snapshot (`publish_snapshot` on
`GuildCreate`/resumed replay); voice, channel and role events for the guild
are dropped until then.

Stall-watchdog actor messages without a `voice_event` field (all in
`run_actor_with_limits` in `crates/bot/src/voice_rooms.rs`, guild-only fields,
no IDs):
`voice worker stuck: queued actions with no progress` (error, at most one per
stuck window): actions are queued but no dispatch turn completed for the full
window; bumps `two_bot_voice_watchdog_stucks_total`.
`voice dispatch turn exceeded observe bound` (warn): one dispatch turn ran
past the observe bound; the turn ran to completion and the queue keeps its
retries; bumps `two_bot_voice_dispatch_timeouts_total`.
`voice worker halt cleared: store credentials healthy` (info): a halted
worker's credential re-probe read the store under timeout, so the
store-CredentialRefused latch cleared; bumps `two_bot_voice_halt_recoveries_total`.
Discord-401 halts never reach the probe (see below), so this line always
means a store recovery, never a token recovery.
`voice halt re-probe skipped: Discord credential latch set` (debug): the
worker halted on a Discord 401 (or the writer latched one); the probe stays
out because only a token fix and a restart clear that halt.
`voice halt re-probe: store still refusing` / `voice halt re-probe timed out`
(debug): the latch stays set; the next probe runs after the re-probe interval.

Creator-view refresh actor messages (all in `GuildRoomWorker::revalidate_creator`
/ `reload_creators` in `crates/bot/src/voice_rooms.rs`, guild-only fields,
no IDs):
`voice creator view revalidated from the store` (info): a join arrived for an
unknown channel id that turned out to be a `voice_creators` row; the row merged
into the in-memory view instead of refusing the join.
`voice creator view merged stored rows` (info, only when at least one new row
merged): a channel create/update/delete (debounced) or the five-minute
maintenance tick merged stored creator rows into the view; the merge only adds
or updates, never deletes.
`voice creator revalidation failed; keeping the cached view` /
`voice creator refresh failed; keeping the cached view` (debug): the store read
failed (or the single-row read-through timed out after 1 s, `timed_out=true`),
so the old view is kept and the join is refused exactly as before.
`voice naming settings refresh failed; keeping the cached view` (warn, in
`GuildRoomWorker::reload_name_settings` in `crates/bot/src/voice_name_panel.rs`):
the `config_snapshot` read failed, so the naming reload kept the cached
settings and creators; the direct creator merge still converges the view while
this fails.

## Unknowns (TBD)

- Whether any additional `tracing` message on the session-commit path was
  added after this extract; re-run the `tracing::(info|warn|error)` search
  before treating this table as exhaustive. Status: TBD.
- Exact JSON shape of the `voice_event` lines (formatting owner separate);
  this catalog records only the field names and message strings. Status: TBD.
- Worker-side `container_gateway_failure` JSON line is emitted by the
  keepalive tick, not Rust; its spelling is recorded in
  `docs/startup-diagnostics.md`, not verified here. Status: TBD.

## Sources

- `crates/bot/src/gateway.rs`, `crates/bot/src/main.rs`,
  `crates/bot/src/server.rs`, `crates/bot/src/shutdown.rs`
- `crates/bot/src/gateway_metrics.rs`, `crates/core/src/metrics.rs`
- `crates/bot/src/gateway_failure.rs`, `docs/startup-diagnostics.md`
- `docs/metrics.md`, `docs/watch-signal-queries.md`,
  `docs/gateway-recovery.md`
- Job producers: `crates/bot/src/feed_jobs.rs`,
  `crates/bot/src/self_role_handlers.rs`, `crates/bot/src/member_runtime.rs`,
  `crates/bot/src/automod_gateway.rs`
- Alert sources: `wrangler/src/alert-rules.ts`,
  `wrangler/src/metrics-scrape.ts`, `crates/core/src/evidence.rs`,
  `docs/runbook.md`
