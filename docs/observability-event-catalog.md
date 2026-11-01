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
| `community facts arming refused; capture disabled` | `crates/bot/src/main.rs` (`run`) | Facts-sink arming refused at boot; capture stays disabled exactly like the scorecard flag being off |
| `gateway shard loop started` | `crates/bot/src/gateway.rs` (`run_shard`) | Reception task entered the Twilight stream loop |
| `gateway reconnect failed; Twilight will retry` | `crates/bot/src/gateway.rs` (`run_shard`) | Transport reconnect attempt failed; Twilight owns the retry |
| `cold resume committed; requesting voice snapshot via identify` | `crates/bot/src/gateway.rs` (`run_shard`) | Cold voice RESUME committed; reception IDENTIFies for a fresh voice snapshot |
| `gateway ready; checkpoint committed` | `crates/bot/src/gateway.rs` (`apply_dispatch`) | Dispatch committed and shard marked Connected |
| `gateway leveling dispatch failed` | `crates/bot/src/gateway.rs` (`apply_dispatch`) | Leveling funnel drain failed inside the checkpoint deadline |
| `gateway leveling dispatch skipped; schema gap` | `crates/bot/src/gateway.rs` (`apply_dispatch`) | Auxiliary XP drain hit `42P01`/`42501`; warns with `drain=leveling`, bounded cause and SQLSTATE, counts `two_bot_gateway_drain_skips_total`, and permits checkpoint advance without promising award replay |
| `gateway leveling collect failed` | `crates/bot/src/gateway.rs` (`requests_or_hold_cursor`) | Leveling funnel collection refused (poisoned deferred buffer); checkpoint cursor held so the worker stops and the supervisor restarts |
| `gateway community facts dispatch failed` | `crates/bot/src/gateway.rs` (`apply_dispatch`) | Community facts drain failed; worker continues and the scorecard fails closed on missing coverage |
| `gateway community facts dispatch timed out` | `crates/bot/src/gateway.rs` (`apply_dispatch`) | Community facts drain missed the checkpoint deadline; worker continues and the scorecard fails closed on missing coverage |
| `gateway community facts buffer poisoned` | `crates/bot/src/gateway.rs` (`hold_cursor_on_facts_poison`) | Community facts buffer poisoned; checkpoint cursor held so the worker stops and the supervisor restarts, unlike an ordinary write failure |
| `gateway dispatch refused: duplicate JSON key` | `crates/bot/src/gateway.rs` (`run_shard`) | MESSAGE_UPDATE bytes carried a repeated JSON key; parse and dispatch skipped with no verdict input, worker still commits the checkpoint so a resume replays nothing |
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
| `worker supervisor ended abnormally during shutdown join` | `crates/bot/src/main.rs` (`join_worker_supervisors`) | A retained worker supervisor (raid, containment or join-risk drain) ended with an error during the shutdown join; `panicked` distinguishes a panic from a cancellation. The remaining supervisors still join |
| `worker supervisor did not finish within the remaining shutdown deadline; aborting it, so its worker drain is cut short` | `crates/bot/src/main.rs` (`join_worker_supervisors`) | Error; the supervisor outlived its share of the shutdown deadline (`budget_ms`, `event = "worker_supervisor"`, `error_class = "worker_shutdown_join_exceeded"`) and was aborted, so its worker's in-flight item and queued inbox are discarded uncounted |
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
| `eventsub delivery refused` | `crates/bot/src/eventsub_http.rs` (`refuse`) | Delivery failed verification, admission or shape; sampled (first, then every 64th process-wide) so hostile bursts count in counters, never per-request logs. `status` and `error_class` name the refusal, bounded `message_type` (`webhook_callback_verification` / `notification` / `revocation` / `unknown`) names the delivery type (`unknown` before headers are established). Classify errors warn through `sampled_classify_warn` instead |
| `eventsub delivery classification failed` | `crates/bot/src/eventsub_http.rs` (`sampled_classify_warn`) | Sampled warn (first, then every 64th process-wide, shared budget with `refuse`) for a `classify()` error; bounded `error_variant` carries the `EventSubError` snake_case label (`wrong_subscription`, `invalid_body`, `invalid_challenge`, or a future variant), `error_class` the outcome (`acknowledged`, `malformed`, `unavailable`), plus `status` and bounded `message_type` |
| `challenge answered` | `crates/bot/src/eventsub_http.rs` (`receive`) | Webhook verification challenge answered with the plaintext challenge; Twitch enables the subscription |
| `notification acknowledged` | `crates/bot/src/eventsub_http.rs` (`handle_notification`, two sites) | Notification verified and acknowledged; when delivery dependencies are injected, the durable claim → send → settle flow has settled (or deduped) before the ack |
| `revocation acknowledged` | `crates/bot/src/eventsub_http.rs` (`receive`) | Revocation verified and acknowledged; boot/periodic reconcile owns re-subscription; bounded `reason` (`user_removed` / `authorization_revoked` / `notification_failures_exceeded` / `version_removed` / `other`) answers counts by reason |
| `delivery ignored` | `crates/bot/src/eventsub_http.rs` (`receive`, one info site) | Unknown message type acknowledged with 2xx; deterministic planning errors warn sampled with their `error_variant` instead, and unknown/future classification errors record their variant label and are refused with retryable 503 |
| `delivery child failed; durable claim fences retain uncertainty` | `crates/bot/src/eventsub_http.rs` (`report_child_failure`) | Warn, emitted from the failing child's own request span when it ends by panic or cancellation (`panicked` / `cancelled` booleans only; no join error or panic payload is logged); any started durable claim remains uncertain and fenced |
| `receiver shutdown deadline exhausted with delivery children unfinished` | `crates/bot/src/eventsub_http.rs` (`join_children`) | Warn; joining children exceeded the receiver's HTTP-drain shutdown budget (`pending` count); children detach instead of aborting and started claims stay fenced |
| `receiver shutdown deadline exhausted; unfinished delivery claims remain fenced` | `crates/bot/src/eventsub_http.rs` (`serve`) | Warn; the receiver exhausted its HTTP-drain shutdown budget, reports failure to its caller, and leaves unfinished claim state fenced |
| `event = "eventsub_receiver"` | `crates/bot/src/eventsub_http.rs` (`serve`, `join_children`, `report_child_failure`, `refuse`, `sampled_classify_warn`, `receive`, `handle_notification`) | Bounded `event` field value every receiver emission carries; no header, token or body leaves the process |
| `eventsub reconcile parked: TWO_EVENTSUB is not exactly 1` | `crates/bot/src/eventsub_reconcile.rs` (`startup_reconcile`, `spawn_if_enabled`) | Info; gate dark at spawn or per pass, no subscription is listed or mutated (`status = "parked"`, `reason = "flag_dark"`) |
| `eventsub reconcile parked: the EventSub receiver is dark, no shared secret snapshot` | `crates/bot/src/eventsub_reconcile.rs` (`spawn_if_enabled`) | Info; enabled but no receiver snapshot at spawn, so no task and no pass (`status = "parked"`, `reason = "receiver_dark"`) |
| `eventsub reconcile refused: callback configuration is invalid` | `crates/bot/src/eventsub_reconcile.rs` (`startup_reconcile`) | Error; `TWO_TWITCH_CALLBACK_URL` missing or not an https callback, no Helix call (`error_class = "reconcile_config_invalid"`) |
| `eventsub reconcile parked: broadcaster allowlist is empty, no subscriptions mutated` | `crates/bot/src/eventsub_reconcile.rs` (`startup_reconcile`) | Warn; `TWO_TWITCH_BROADCASTERS` empty, pass parked without mutation (`error_class = "reconcile_no_broadcasters"`) |
| `eventsub reconcile refused: Twitch credentials are missing or invalid` | `crates/bot/src/eventsub_reconcile.rs` (`startup_reconcile`) | Error; Operator Twitch client credentials unreadable, no Helix call (`error_class = "reconcile_config_invalid"`) |
| `eventsub reconcile refused: webhook secret snapshot is unreadable` | `crates/bot/src/eventsub_reconcile.rs` (`startup_reconcile`) | Error; live signing snapshot unreadable, no Helix call (`error_class = "reconcile_config_invalid"`) |
| `eventsub reconcile found failed subscriptions on our callback; deleting and re-creating` | `crates/bot/src/eventsub_reconcile.rs` (`reconcile_all_with_metrics`) | Error; failed subscriptions on our callback counted before the pass (`error_class = "reconcile_prior_failed"`), then deleted and re-created |
| `eventsub subscriptions re-created but not all verified yet` | `crates/bot/src/eventsub_reconcile.rs` (`reconcile_all_with_metrics`) | Warn; pass re-created subscriptions but at least one desired broadcaster is not `enabled` yet (`status = "pending"`, `error_class = "reconcile_pending"`) |
| `eventsub subscriptions reconciled` | `crates/bot/src/eventsub_reconcile.rs` (`reconcile_all_with_metrics`) | Info; every desired broadcaster holds an `enabled` subscription on our callback (`status = "healthy"`) |
| `eventsub reconcile action failed; continuing with the rest of the plan` | `crates/bot/src/eventsub_reconcile.rs` (`run_plan`) | Error; one delete or create was rejected or failed in transport (`action = "delete"` or `"create"`, `error` names the status, `error_class = "reconcile_action_failed"`); the later actions and the follow-up listing still run |
| `eventsub reconcile did not converge: an action failed or a healthy subscription is missing` | `crates/bot/src/eventsub_reconcile.rs` (`reconcile_all_with_metrics`) | Error; after the follow-up listing, at least one action failed or a desired broadcaster lacks a healthy subscription; the pass fails loudly with `failed` and `missing` counts (`error_class = "reconcile_unhealthy"`) |
| `eventsub reconcile failed` | `crates/bot/src/eventsub_reconcile.rs` (`startup_reconcile`) | Error; the pass returned an error after the gate and config checks (`error_class = "reconcile_failed"`) |
| `eventsub reconcile pass interrupted by shutdown; remaining actions skipped and subscription state unverified until the next startup pass` | `crates/bot/src/eventsub_reconcile.rs` (`log_interrupted`) | Warn; shutdown latched between Helix calls, so remaining actions and verification were skipped (`completed`/`remaining` action counts, `error_class = "reconcile_shutdown_interrupted"`); the next startup pass re-lists and re-plans |
| `eventsub reconcile task ended abnormally` | `crates/bot/src/eventsub_reconcile.rs` (`join`) | Error; the joined reconcile task panicked or was cancelled (`panicked` says which, `error_class = "reconcile_task_failed"`) |
| `eventsub reconcile task did not finish within the shutdown drain; aborting it, so an in-flight remote mutation outcome is uncertain until the next startup pass re-checks` | `crates/bot/src/eventsub_reconcile.rs` (`join`) | Error; the reconcile task outlived the bounded shutdown drain (`drain_ms`) and was aborted, so an in-flight subscription mutation may or may not have landed (`error_class = "reconcile_shutdown_drain_exceeded"`); the next startup pass settles the real state |
| `event = "eventsub_reconcile"` | `crates/bot/src/eventsub_reconcile.rs` (`reconcile_all`, `reconcile_all_with_metrics`, `run_plan`, `log_interrupted`, `startup_reconcile`, `spawn_if_enabled`, `join`) | Bounded `event` field value every reconcile emission carries; scalar counts, statuses and error classes only, never a secret, header, challenge or body |

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
| `two_bot_process_incarnation{part}` | Process-lifetime gauge: four fixed `part` labels (`0`, `1`, `2`, `3`) carry unsigned 32-bit numeric limbs of a nonzero random identity, stable across renders and new on process replacement. No PID, timestamp, build or identity-valued label. Optional for older containers; partial, duplicate, mislabeled or invalid parts invalidate the scrape. A validated same-scrape identity attributes gateway pairing windows, not health or recovery; full semantics live in `docs/metrics.md` |
| `two_bot_gateway_reconnects_total` | New HELLOs after the first in the running loop |
| `two_bot_gateway_resumes_total` | Received RESUMED dispatches |
| `two_bot_gateway_disconnects_total` | Transport losses through the shard supervisor; each must pair with a later RESUME or fresh READY |
| `two_bot_gateway_drain_skips_total` | Label-free auxiliary leveling drain skips for missing tables/grants; the `gateway_drain_skips` ticket consumes any increase. First sample and resets baseline, missing/invalid family is no window; see [bounded response](runbook.md#alert-gateway-drain-skips). No deployment is claimed |
| `two_bot_gateway_missed_events_total` | Sequence gaps inside one session; any nonzero increase fails the zero-missed-events acceptance |
| `two_bot_gateway_rsvp_refused_total` | RSVP interactions refused at reception because the in-flight RSVP bound (32) was exhausted: no prepare task spawned and nothing entered the serial drain. Non-RSVP interactions bypass admission and never increment it |
| `two_bot_gateway_latency_seconds` | Last heartbeat ACK round-trip; `NaN` until measured |
| `two_bot_gateway_prefix_trigger_refused_total{reason="verdict"}` | In-scope prefix candidates refused by the automod verdict; the worker records candidates even when its capture-only gate prevents detached dispatch. Unmatched content, scope mismatches, bot/webhook messages and disabled text-command gates never increment it |
| `two_bot_presence_capture_total{outcome}` | Presence-update capture decisions on the hot path; `outcome` is `inserted` (first delivery stored a row), `duplicate` (same-key retry absorbed by the dedup key) or `rejected` (refused before the insert). Deliberately unconsumed: no production callsite records the family yet |
| `two_bot_automod_decisions_total{outcome}` | Terminal automod inspection outcomes; `outcome` is `accepted`, `already_processed`, `dry_run`, `protected`, `deleted`, `warned`, `timed_out` or `sanction_refused`. Recorded once per committed `StoredOutcome` by the shared activation; replays and retained claims never count. Fixed eight series from process start; no message text, excerpts or user IDs become labels. `sanction_refused` is consumed by `automod_sanction_refused` (tickets on any increase); the other seven outcomes are policy working as designed |

## Raid and join-risk observer drops

Source: `RaidRuntime::observe_join` in `crates/bot/src/raid_runtime.rs` and
`JoinRiskRuntime::observe_join` in `crates/bot/src/join_risk_runtime.rs`.
The fixed `JoinQueueDropReason` vocabulary and exposition live in
`crates/core/src/metrics.rs` (`raid_dropped_join`, `join_risk_dropped_join`,
`render_with_rest_guard`).

| Series | Meaning |
| --- | --- |
| `two_bot_raid_dropped_joins_total{reason}` | Joins not enqueued for raid observation: `queue_full` (WARN, no wait) or `queue_closed` (ERROR with `error_class="raid_queue_closed"`). Each dropped join counts once; both series render zero initially and reset on process restart |
| `two_bot_join_risk_dropped_joins_total{reason}` | Joins not enqueued for risk scoring: the same fixed reasons; Closed logs ERROR with `error_class="join_risk_queue_closed"`. Each dropped join counts once; both series render zero initially and reset on process restart |

No guild/member/error-text labels or alert rules are added. Observer drops
happen after gateway persistence and are not retried. See
[queue triage](runbook-observability.md#raid-and-join-risk-queue-drops).

## Automod facts read warnings

Source: `RestAutomodFacts::get` in `crates/discord/src/automod_activation.rs`.

| Event | Callsite (stable anchor) | Level | Fields | Meaning |
| --- | --- | --- | --- | --- |
| `automod_facts_read_failed` | `crates/discord/src/automod_activation.rs` (`RestAutomodFacts::get`) | WARN | `step`: `guild`, `bot_member`, `target_member` or `message`; `cause`: the fixed vocabulary from `DiscordError::cause()` in `crates/discord/src/executor.rs` | A strict Discord facts read failed; the caller still returns unavailable facts and preserves the existing fail-closed decision. Enrichment's author-member read uses `target_member`. |

Only `Err` emits: successful reads and 404 (`Ok(None)`) stay silent and do not
consume the warning window. The first failure emits immediately, then at most
one warning per 60 seconds per facts resolver, shared across all steps and
guilds. This constant-memory sampler also bounds each guild's warnings; a
failure in one guild can suppress another guild's warning in the same window.
Suppressed failures do not extend the window. The event carries no path,
guild/member/message IDs, response body or formatted error. It adds no metric
or retry and does not change `ActivationOutcome::Unavailable` or the eight
terminal decision outcomes above. Missing or incomplete facts can still yield
`Unavailable` without this warning; the warning is a sampled error signal,
not a count of unavailable decisions.

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
| `two_bot_voice_worker_halted` | None (label-free gauge) | Optional scrape input: validated when present (`ALERT_OPTIONAL_INPUT_LABELS`, pinned 0/1); older containers without it still scrape | Consumed by `voice_worker_halted`: pages while 1; a valid 0 clears, missing/invalid holds | 1 while any current guild actor is halted by a store CredentialRefused or Discord 401, otherwise 0; a healthy guild cannot hide another guild's halt |
| `two_bot_voice_queue_pending` | Fixed `lane`: `urgent` or `deferred` (exactly two series) | Additional series: accepted but not required by the scrape validator, which pins only the alert-input label sets; read manually from the container exposition or the authenticated pull | Deferred: depth without age is not a failure signal — bursts queue legitimately and no code-grounded bound exists (depths are unbounded saturating sums); member impact is covered by the join-budget burn rules and `voice_queue_stall`. Manual depth complement to the stall ticket; revisit when production depth distribution justifies a bound | Queued (not in-flight) voice actions per lane, saturating-summed across all current guild actors |
| `two_bot_voice_queue_oldest_age_seconds` | None (label-free gauge) | Optional scrape input: validated when present (`ALERT_OPTIONAL_INPUT_LABELS`); older containers without it still scrape | Consumed by `voice_queue_stall`: tickets past 120 s (matches the in-process stuck horizon; the watchdog sibling recomputes the age from the snapshot's first-enqueue stamp, so a hung dispatch still tickets past the bound); at or below clears, missing/invalid holds | Age in seconds of the oldest queued voice action, the maximum across all current guild actors; 0 when every queue is empty |
| `two_bot_voice_live_not_ready_seconds` | None (label-free gauge) | Optional scrape input: validated when present (`ALERT_OPTIONAL_INPUT_LABELS`); older containers without it still scrape | Consumed by `voice_live_not_ready`: pages past 120 s, promoting the host-watchdog bound into the standard pipeline; at or below clears, missing/invalid holds; the watchdog sibling reads the shared live view directly, so a hung dispatch still pages past the bound | Seconds a guild's live voice view has been not ready while the gateway is connected (READY or RESUMED handled), the maximum across all current guild actors; 0 when every view is ready or the gateway is down. While above 0 that guild's creator joins are dropped without an accept or refusal line |
| `two_bot_voice_live_self_heal_total` | None (label-free counter) | Optional scrape input: validated when present (`ALERT_OPTIONAL_INPUT_LABELS`); older containers without it still scrape | Consumed by `voice_live_self_heal`: tickets on any increase between valid samples; first sample baselines, restarts re-baseline, a missing family is no window | Live voice views republished by the not-ready-while-connected self-heal |
| `two_bot_voice_inbox_overflow_dropped_total` | None (label-free counter) | Additional series: accepted but not required by the scrape validator; read manually from the container exposition or the authenticated pull | Consumed by no alert rule; compare increases with queue age and dispatch progress to diagnose overload | Capacity refusals at the 256-entry actor inbox gate, including public ballot message references; accepted/lossless sends and closed-channel rollbacks do not count. Counted once at source, retained across actor retirement, reset on process restart |
| `two_bot_voice_queue_overflow_dropped_total` | None (label-free counter) | Same additional-series handling as above | Consumed by no alert rule; zero does not imply healthy drainage or a production memory bound | Capacity refusals at the opt-in 1,024-pending action intake; coalescing and always-admit `enqueue` do not count. All current production producers use uncapped `enqueue`, so this series remains zero there. Counted once at source, retained across queue retirement, reset on process restart |
| `two_bot_voice_actor_inbox_turn_bound` | None (label-free gauge) | Same additional-series handling as above | Consumed by no alert rule; manual check that the published bound is the expected 32 | Fair-scheduling bound: at most this many consecutive guild-actor inbox turns before a ready dispatch, timer or maintenance branch runs; published once per actor at startup, last writer wins, 0 until the first actor publishes |
| `two_bot_voice_actor_inbox_turns_total` | None (label-free counter) | Same additional-series handling as above | Consumed by no alert rule; compare with the dispatch-turns series to spot churn-drain | Guild-actor inbox commands processed, saturating-summed across all guild actors. Inbox turns climbing while dispatch turns stay flat means the actors are draining churn without dispatching |
| `two_bot_voice_actor_dispatch_turns_total` | None (label-free counter) | Same additional-series handling as above | Consumed by no alert rule; dispatch-progress complement to the inbox-turns series | Guild-actor dispatch turns that ran, saturating-summed across all guild actors, whether or not the turn progressed |

## EventSub delivery series

Counters emitted by the Twitch EventSub receiver render. Unknown `kind`
and `outcome` values collapse to `other`, and no member ID, token, header,
body, broadcaster, subscription or secret data reaches any label.

| Series | Meaning |
| --- | --- |
| `two_bot_eventsub_received_total` | Verified-shape EventSub deliveries accepted for signature verification, including tampered signatures; requests rejected before body intake are not deliveries. Consumed by `eventsub_receive_gap`: tickets when received exceeds the summed classified kinds for 3 consecutive samples |
| `two_bot_eventsub_classified_total{kind}` | Deliveries by bounded receiver classification; `kind` is `challenge`, `notification`, `revocation`, `tampered`, `ignored`, one of the thirteen `EventSubError` snake_case labels (`invalid_secret`, `invalid_header`, `body_too_large`, `bad_signature`, `invalid_timestamp`, `stale_timestamp`, `invalid_body`, `duplicate_keys`, `wrong_subscription`, `invalid_challenge`, `invalid_callback`, `invalid_broadcaster_id`, `too_many_broadcasters`) or `other`; classify errors record their variant label, and `other` absorbs only unknown label strings |
| `two_bot_eventsub_posts_total{outcome}` | Go-live post attempts by bounded outcome; `outcome` is `attempted`, `succeeded`, `failed` or `other`; zero until the go-live sender slice is wired |
| `two_bot_eventsub_reconcile_passes_total{outcome}` | Reconcile passes by bounded outcome; `outcome` is `healthy`, `pending` or `failed`; exactly one sample per pass that reaches Helix, parked or refused startup passes stay log-only |
| `two_bot_eventsub_reconcile_actions_total{action}` | Successful reconcile mutations by bounded action; `action` is `create` or `delete`; 409 already-exists confirmations fold into the pass outcome. Deliberately unconsumed: successes are the healthy signal |
| `two_bot_eventsub_reconcile_errors_total{error_class}` | Reconcile failure sites by bounded log `error_class`; `error_class` is `reconcile_action_failed`, `reconcile_prior_failed`, `reconcile_unhealthy`, `reconcile_failed` or `other`; no status, body or response bytes in labels. `reconcile_prior_failed` is consumed by `eventsub_reconcile_prior_failed` (tickets on finding growth after a healthy/pending completion with no failed-pass growth; mid-pass findings wait for an outcome); the other sites pair with a failed pass and ticket through `eventsub_reconcile_failed` |

## Feed fetch series

Counters emitted by the feed poller render. Unknown `outcome` values collapse
to `failure` and unknown `error_class` values collapse to `other`, and no
source URL, host, header, body or feed id reaches any label.

| Series | Meaning |
| --- | --- |
| `two_bot_feed_fetches_total{outcome}` | Feed fetch attempts by bounded outcome; `outcome` is `success` (2xx fetch), `invalid_source` (source refused before dialling), `refused_redirect` (redirect refused by policy) or `failure` (every other failure, including non-2xx HTTP). Deliberately unconsumed: the errors sum below carries the same signal with finer reason codes |
| `two_bot_feed_fetch_errors_total{error_class}` | Failed feed fetches by bounded error class; `error_class` is `invalid_source`, `non_public_address`, `redirect_refused`, `too_large`, `invalid_encoding`, `unsupported_content_type`, `unsupported_encoding`, `resolve`, `transport`, `deadline`, `http_error` (completed non-2xx fetch) or `other`; successes record nothing here. Consumed by `feed_fetch_failures`: tickets on any summed increase between valid samples |

## Moderation enforcement series

Counters emitted by the moderation enforcement render. Unknown outcomes
collapse to `other` (unknown verbs collapse to the trailing verb slot,
`unlock`, until the verb allowlist grows), and no member ID, reason text
or message content reaches any label. Verbs are the bounded `ModerationAction`
command vocabulary (`ban`, `tempban`, `kick`, `timeout`, `warn`, `purge`,
`slowmode`, `lockdown`, `unlock`); outcomes are `success`, `refused`,
`discord_error`, `storage_error` and `other`. Each terminal member or
channel enforcement counts once; retries and rate-limit backoffs are not
outcomes. See `docs/metrics.md` for the full label table.

| Series | Meaning |
| --- | --- |
| `two_bot_moderation_enforcements_total{verb,outcome}` | Terminal moderation enforcements by bounded verb and outcome. `discord_error`/`storage_error` are consumed by `moderation_enforcement_failures` (tickets on any summed increase); `success`, `refused` and `other` stay quiet |

## Ticket lifecycle series

Counter emitted by the ticket runtime in `crates/bot/src/ticket_runtime.rs`
(`emit_lifecycle` and `purge`) and rendered by
`crates/core/src/metrics.rs`. Every retire, abandon, purge and successful
staff-button claim/close executed through the button path counts once with
a fixed-vocabulary cause; the purge pass counts one per deleted transcript
body and reports a `db_cause` outcome when its storage write fails.
Recovery completions and the open-path controls-post rollback are not
instrumented. No member text, channel name, ticket id or error string ever
becomes a label. No alert rule consumes this family yet; read it alongside
the `ticket_lifecycle` log lines. Full series, label and cause vocabularies
live in `docs/ticket-lifecycle-observability.md` and
`docs/metrics.md`.

| Series | Meaning |
| --- | --- |
| `two_bot_ticket_lifecycle_total{event,outcome}` | Terminal ticket lifecycle events by bounded event (`retire`, `abandon`, `purge`, `claim`, `close`) and outcome (success outcomes plus `db_cause` classes) |

## Retention replay series

Label-free counter emitted by the retention purge-skip path in
`crates/bot/src/gateway.rs` (`retention_lifecycle`) and rendered by
`crates/core/src/metrics.rs`. No guild id, SQLSTATE or error text ever
becomes a label. Consumed by `retention_replay_needed`: tickets on any
increase between valid samples (first sample is baseline-only, restarts
re-baseline, missing family is no window). Full semantics live in
`docs/metrics.md`.

| Series | Meaning |
| --- | --- |
| `two_bot_retention_replay_needed_total` | Skipped retention purges needing Operator replay: one per real guild removal whose purge-queue write hit the schema gap; successful queue writes and rejoin-cancel skips never increment it |

## Containment store error series

Counter emitted by the containment completion path in
`crates/bot/src/containment_runtime.rs` (`finish`) and rendered by
`crates/core/src/metrics.rs` (`render`). A failed `complete_incident` also
surfaces as outcome-unknown in the staff alert, so the alert and the durable
record never silently disagree. Unknown ops collapse to `other`; no error
text, query or identifier ever becomes a label. Manual check, not an alert
input: consumed by no alert rule. Full semantics live in `docs/metrics.md`.

| Series | Meaning |
| --- | --- |
| `two_bot_containment_store_errors_total{op}` | Containment-store failures by bounded operation; `op` is `complete_incident` or `other` |

## Drop-guard poison recovery series

Labelled counter recorded by `recover_drop_poison` in
`crates/core/src/metrics.rs` and rendered by `render` in the same file. Each
poisoned drop counts once under its guard and logs one `drop guard recovered
a poisoned lock` warn carrying the guard name; the drop still releases its
state from the recovered guard. Unknown guards collapse to the trailing
`voice_connection_guard` slot, and no panic payload, ID, token or body content ever
becomes a label. Manual check, not an alert input: consumed by no alert rule.
Full semantics live in `docs/metrics.md`.

| Series | Meaning |
| --- | --- |
| `two_bot_drop_poison_total{guard}` | Poisoned locks recovered by drop guards instead of panicking inside `drop`; `guard` is `command_dispatch_guard`, `schedule_run`, `user_slot`, `in_flight_guard`, `ticket_supervisor`, `response_accounting` or `voice_connection_guard`. Any increase is a fault worth a look in the container logs |

## Scheduled job labels

Each label owns independent `two_bot_job_runs_total{job,outcome}`,
`two_bot_job_last_success_timestamp_seconds{job}`,
`two_bot_job_consecutive_failures{job}` and
`two_bot_job_last_error_class{job,class}` series (see the job allowlist
`JOBS` in `metrics.rs`). Success in one label never clears another label's
streak. The first four labels below have no `JOB_INTERVAL_SECONDS` cadence
entry, so staleness tickets via `job_unknown_stale` instead of `job_stale`.
`lfg_sweep` has a 60-second cadence entry and uses `job_stale`.

| Job label | Producer (stable anchor) | Meaning |
| --- | --- | --- |
| `feeds` | `feed_jobs.rs` (`scheduled_job_live` via `register` / `register_fenced`) | Feed poller sweep on the boot-configured interval with a live inner polling gate; gated skips return success and reset the streak |
| `self_role_recovery` | `self_role_handlers.rs` (`recovery_job`) | Self-role repair sweep every 30 s; a successful sweep need not settle every pending repair; failure-only alerting, no `job_stale` paging |
| `member_unban_sweep` | `member_runtime.rs` (`sweep_job`) | Member unban sweep every 30 s with an attempt timeout that may outlive its cadence; a successful sweep need not complete any unban; failure-only alerting |
| `automod_expiry` | `automod_gateway.rs` (`expiry_job`) | Automod repeat-history expiry tick every 60 s; a gateway without built activation is a no-op success, and real expiry work runs only with the automod gate on; failure-only alerting |
| `lfg_sweep` | `lfg_jobs.rs` (`register`) | Bounded LFG lifecycle sweep every 60 s under announcement/identity gates; at most 20 candidates, row-local failures continue, and success does not prove every row or message refresh settled; 60 s `job_stale` cadence |

## Alert rules

Rule ids evaluated in `alert-rules.ts` (`evaluateMetrics`) against the
scrape inputs pinned in `metrics-scrape.ts` (`ALERT_INPUT_LABELS`, plus
`ALERT_OPTIONAL_INPUT_LABELS` for optional families). The id
is the single shared spelling also pinned in `evidence.rs`
(`ALERT_RULE_IDS`), so soak evidence packets attribute correctly.

| Rule | Input series | Meaning |
| --- | --- | --- |
| `rest_5xx_rate` | `two_bot_rest_requests_total{result="5xx"}` and all `two_bot_rest_requests_total` samples | Pages when at least 10 new REST requests arrive between samples and more than 10% are 5xx; a counter reset suppresses the window. |
| `job_unknown_stale` | `two_bot_job_last_success_timestamp_seconds{job}` for labels outside `JOB_INTERVAL_SECONDS`, `TICKER_STALE_JOBS` and `UNKNOWN_STALE_EXCLUDED` | Ticket when a job without a cadence entry has a nonzero success timestamp older than 2 hours; never-succeeded stays silent; `session_checkpoint` is event-driven and excluded |
| `gateway_unpaired_disconnect` | `two_bot_gateway_disconnects_total` against `two_bot_gateway_resumes_total` plus fresh `two_bot_gateway_events_total{event="READY"}`, attributed by optional `two_bot_process_incarnation{part}` | Pages after 3 consecutive valid outstanding-loss samples: disconnect growth opens the loss and flat counters keep it outstanding until RESUMED or fresh READY growth clears it (pairing in `pairGatewayDisconnects`); readiness alone never clears it. When the validated process identity changes or a counter decreases, the evaluator re-baselines and holds an existing page for one sample; the next valid flat sample clears it. First identity adoption into legacy state is not recovery; missing identity after attribution or invalid inputs hold the previous state. The disconnect counter increments in `gateway.rs` (`transport_disconnected`) |
| `gateway_not_ready` | `/readyz` `components` gateway entry (parsed by the keepalive probe, not a `/metrics` series) with the pairing counters as the restart signal | Pages after 3 consecutive valid samples with the gateway component `starting`/`down` and no disconnect edge required (stuck starting, parked, or draining); any ready sample clears at once; invalid probes and failed scrapes hold the previous state |
| `voice_join_fast_burn` | `two_bot_voice_joins_received_total` minus benign `duplicate` refusals in `two_bot_voice_join_refused_total` against `two_bot_voice_join_to_move_seconds_bucket{le="5"}` | Pages when the creator-join budget (99.5% moved within 5 s) burns faster than 14.4x in both the 1 h and 5 min windows; every non-duplicate refusal, failed move and slow (>5 s) move is burn; each window needs at least 10 genuine joins and two bad moves, receive counts lag one scrape, and incomplete post-restart windows stay inactive; missing inputs hold the previous state |
| `voice_join_slow_burn` | Same inputs as `voice_join_fast_burn` | Ticket, not page: same budget burning faster than 6x in both the 6 h and 30 min windows with the same per-window volume and bad-move floors; catches the sustained grind the fast rule never spikes on; do not close as "fast burn resolved" |
| `voice_creator_map_stale` | `two_bot_voice_creator_map_age_seconds` with `two_bot_voice_creator_map_generation` | Pages when the age since the last successful creator-map merge passes 600 s (two missed 5 min refreshes) after at least one merge; generation 0 (never merged: boot or voice parked) never fires; the gauges are optional scrape inputs, so older containers hold the previous state |
| `community_facts_drain_failures` | `two_bot_community_facts_drain_failures_total{reason}` for `reason` in `error`, `timeout`, `other` (optional scrape input; a rollback container without it holds the previous state) | Ticket, not page: summed per-reason growth across 3 consecutive completed windows after a baseline sample; a flat sample or counter reset clears the streak; missing/invalid samples break growth evidence but never falsely resolve an active ticket; a single failed drain never pages |
| `eventsub_post_failed_share` | `two_bot_eventsub_posts_total` by fixed `outcome` (`attempted`, `succeeded`, `failed`, `other`) | Ticket, not page: failed posts exceed 10% of attempted posts with at least 10 attempted (sub-floor windows hold the baseline and accumulate across consecutive valid samples until the floor is met); the first valid sample is baseline only, missing/invalid samples preserve, any backwards counter rebaselines, and flat/sub-floor windows hold an active ticket until a judged window clears it |
| `event_store_consumer_lag` | `two_bot_event_store_consumer_lag_seconds` | Ticket, not page: lag above 60 s (one keepalive window, fractional counts) in 3 consecutive valid samples; a valid sample at or below 60 s resolves and rebaselines, missing/invalid samples and failed scrapes break the streak but preserve a firing ticket; the gauge is an optional scrape input, so older containers without it reset `eventStoreLagStreak` to `0` while retaining an existing firing ticket |
| `error_budget_fast_burn` | All `two_bot_rest_requests_total` samples (eligible requests) against `result="5xx"` plus `result="transport"` (budget spend) | Pages when the REST error budget (99.9% success over 48 h) burns faster than 12x in both the 15 min and 2 min windows; each window needs at least 10 eligible requests and two budget errors; incomplete post-restart windows stay inactive; missing or malformed inputs hold the previous state |
| `error_budget_slow_burn` | Same inputs as `error_budget_fast_burn` | Ticket, not page: same budget burning faster than 3x in both the 2 h and 15 min windows with the same per-window volume and error floors; needs full 2 h history, holds on missing inputs, scrape gaps, or counter resets, and resolves on a covered quiet or below-threshold 15 min window |
| `rest_admission_breaker_open` | `two_bot_rest_guard_breaker_open` | Ticket, not page: the label-free 0/1 gauge reads 1 for 3 consecutive samples; one valid closed sample recovers, missing/invalid samples hold a firing ticket without manufacturing evidence; the gauge is an optional scrape input, so rollback containers hold the previous state |
| `audit_delivery_halt` | `two_bot_audit_delivery_halt` with `two_bot_job_consecutive_failures{job="audit_retry"}` and `two_bot_job_last_success_timestamp_seconds{job="audit_retry"}` | Ticket when the label-free halt gauge reads `1` for 3 consecutive valid samples (about two minutes at the 60 s cadence); any nonzero `audit_retry` failure count vetoes both firing and recovery, and missing, duplicate, labeled or non-boolean readings break the streak while holding an active ticket; a valid `0` after a healthy sweep resolves once, and a boot-default zero cannot resolve until `audit_retry` has a nonzero success timestamp |
| `eventsub_tampered` | `two_bot_eventsub_classified_total{kind="tampered"}` | Pages when the `tampered` counter (only `BadSignature`) rises in 3 consecutive samples; the first sample only baselines, a flat window resolves, a reset rebaselines, and missing/invalid scrapes hold firing |
| `eventsub_reconcile_failed` | `two_bot_eventsub_reconcile_passes_total{outcome}` with the fixed `healthy`/`pending`/`failed` outcomes | Ticket, not page: fires when the failed-pass counter grows between valid samples, even beside healthy growth; the first valid sample only stores a baseline, healthy-only or pending-only growth never fires, and a flat failed counter never recovers (only healthy growth with no failed growth resolves, once); any backwards counter re-baselines: the reset sample fires when the new process already counted a failed pass (failed startup pass) and otherwise holds a firing ticket instead of fabricating a transition; missing or invalid samples hold the previous state; the family is an optional scrape input, so older containers hold the previous state |
| `prefix_trigger_verdict_refusals` | `two_bot_gateway_prefix_trigger_refused_total{reason="verdict"}` | Ticket when the verdict-refused prefix count rises in 3 consecutive keepalive samples after a baseline; first sample and restarts clear the streak; `other` is normal refusal volume and never counts; missing inputs hold the previous state |
| `voice_worker_halted` | `two_bot_voice_worker_halted` | Pages while the latch reads 1 (any guild actor halted by a store CredentialRefused or Discord 401); a valid 0 clears at once; missing/invalid holds the previous state |
| `voice_live_self_heal` | `two_bot_voice_live_self_heal_total` | Ticket, not page: any increase between valid samples (one covered fault, already repaired); first sample is baseline-only, restarts re-baseline, a missing family is no window |
| `voice_live_not_ready` | `two_bot_voice_live_not_ready_seconds` | Pages past 120 s, promoting the host-watchdog bound into the standard pipeline (joins are dropped while above 0); at or below clears, missing/invalid holds; the sibling keeps the age climbing through a hang |
| `voice_queue_stall` | `two_bot_voice_queue_oldest_age_seconds` | Ticket, not page: oldest-wait past 120 s (matches the in-process stuck horizon; the sibling keeps the age climbing through a hang); at or below clears, missing/invalid holds |
| `gateway_drain_skips` | `two_bot_gateway_drain_skips_total` (label-free counter; optional scrape input, so older containers without it hold the previous state) | Ticket, not page: any increase between valid samples; each skip represents one auxiliary leveling drain with SQLSTATE `42P01` or `42501`. First samples and resets establish a baseline silently. Missing or invalid samples preserve prior state and baseline without judging a window; a flat valid sample resolves. See the [bounded response](runbook.md#alert-gateway-drain-skips); rule code only, no deployment claimed |
| `retention_replay_needed` | `two_bot_retention_replay_needed_total` (label-free counter; optional scrape input, so older containers without it hold the previous state) | Ticket, not page: any increase between valid samples (one ticket per skipped purge); the first valid sample only stores the baseline, restarts re-baseline, and a missing or malformed family is no window |
| `voice_dispatch_timeouts` | `two_bot_voice_dispatch_timeouts_total` (label-free counter; optional scrape input, so older containers without it hold the previous state) | Ticket, not page: any increase between valid samples (one ticket per over-budget turn); the turn ran to completion and the queue keeps its retries; baseline, restart and gap semantics mirror `retention_replay_needed` |
| `voice_watchdog_stucks` | `two_bot_voice_watchdog_stucks_total` (label-free counter; optional scrape input, so older containers without it hold the previous state) | Ticket, not page: any increase between valid samples (one ticket per 120 s stuck window); baseline, restart and gap semantics mirror `retention_replay_needed` |
| `voice_halt_recoveries` | `two_bot_voice_halt_recoveries_total` (label-free counter; optional scrape input, so older containers without it hold the previous state) | Ticket, not page: any increase between valid samples (one ticket per covered store halt, including brief halt/recover cycles the sampled halt latch never shows; Discord-401 halts never count); baseline, restart and gap semantics mirror `retention_replay_needed` |
| `rest_breaker_opens` | `two_bot_rest_guard_breaker_opens_total` (label-free counter; optional scrape input, so older containers without it hold the previous state) | Ticket, not page: any increase between valid samples (one ticket per trip, like its sustained-open sibling); baseline, restart and gap semantics mirror `retention_replay_needed` |
| `rest_global_pauses` | `two_bot_rest_guard_global_pauses_total` (label-free counter; optional scrape input, so older containers without it hold the previous state) | Ticket, not page: any increase between valid samples (one ticket per new episode; extensions never count); baseline, restart and gap semantics mirror `retention_replay_needed` |
| `gateway_latency_dead` | `two_bot_gateway_latency_seconds` (required scrape input; NaN until the first ACK on the connection) | Pages after 3 consecutive valid samples with no heartbeat-ACK evidence (NaN or an unchanged finite value); any fresh reading clears at once; first sample is baseline-only, missing/malformed samples hold, failed scrapes re-baseline the streak and hold a firing page; co-firing with `gateway_not_ready` means never-connected, a lone page means a half-dead connection |
| `gateway_reconnect_flapping` | `two_bot_gateway_reconnects_total` (required scrape input) | Pages when successful transport reconnections rise in 3 consecutive samples; single blips stay log-only, a flat window or counter reset clears, the first sample is baseline-only, missing/invalid samples hold a firing page |
| `handler_duration_saturated` | `two_bot_handler_duration_seconds_bucket{le}` (required scrape input; `_sum`/`_count` stay unconsumed passthrough) | Ticket, not page: window p99 past 1 s (past-bound share above 1% with at least 10 completions and 2 slow) in 3 consecutive windows; a judged-quiet window or counter reset clears, the first sample is baseline-only, missing/invalid samples drop the baseline and hold a firing ticket; routine quantile movement stays log-only in the query pack |
| `job_failure_share` | `two_bot_job_runs_total{job,outcome}` for the fixed `JOBS` labels and `success`/`failure` outcomes (required scrape input) | Ticket, not page: per-job failures exceed 10% of recent runs with at least 10 runs and 2 failures in the judged window (sub-floor windows accumulate across consecutive valid samples); only a judged window at or below the line resolves, flat/sub-floor windows and missing/invalid samples hold, restarts rebaseline; co-firing with `job_consecutive_failures` means continuous outage, this ticket alone means intermittent sickness |
| `rest_token_invalid` | `two_bot_rest_guard_token_invalid` | Pages on any valid sample reading 1: the bot-token latch is set only by a bot-authenticated 401 and defaults to 0 at boot, so a 1 is never a boot artifact and the first valid sample may fire; a valid 0 (only a new process without the latch) resolves, missing/invalid samples hold the page; the latch is an optional scrape input, so older containers hold the previous state |
| `rest_refusal_surge` | `two_bot_rest_guard_invalid_requests_total` plus `two_bot_rest_guard_rejected_requests_total` | Pages when the combined invalid-observation and local-refusal growth reaches 10+ between two consecutive valid samples; either family can surge alone and both burn the same IP-invalid budget toward the breaker trip; the first valid sample only stores a baseline, a backwards counter re-baselines only that family and contributes no growth for the window while the other family's growth is still judged (a one-step drop on invalid is a 429 scope retraction, not a restart), below-threshold windows resolve, and missing/invalid samples keep the baseline and hold a firing page; both counters are optional scrape inputs, so older containers hold the previous state |
| `rest_global_responses_stuck` | `two_bot_rest_guard_pending_global_responses` | Pages when the pending global-body count stays nonzero for 3 consecutive valid samples (a pending body holds admission even at zero remaining, and the 5 s body deadline makes three stuck samples 24-36x past the expected drain); a valid zero resolves, missing/invalid samples and failed scrapes break the streak but preserve a firing page; the gauge is an optional scrape input, so older containers reset the streak while retaining a firing page; the paused/remaining gauges stay out of this rule by design (a live deadline is healthy backpressure and the countdown cannot wedge) |
| `feed_fetch_failures` | `two_bot_feed_fetch_errors_total{error_class}` summed over the twelve fixed classes (optional scrape input, so older containers without it hold the previous state) | Ticket, not page: any summed increase between valid samples (one ticket per window with new failures); the family records failures only, so even `other` counts; the first valid sample only stores the baseline, restarts re-baseline, and a missing or malformed family is no window |
| `eventsub_receive_gap` | `two_bot_eventsub_received_total` against summed `two_bot_eventsub_classified_total{kind}` (both optional scrape inputs) | Ticket, not page: received ahead of classified for 3 consecutive valid samples (one gapped scrape can land mid-delivery); a closed gap or joint reset resolves, a negative gap is corrupt input and no window, and missing/invalid samples break the streak but preserve a firing ticket |
| `eventsub_reconcile_prior_failed` | `two_bot_eventsub_reconcile_errors_total{error_class="reconcile_prior_failed"}` with all three `two_bot_eventsub_reconcile_passes_total{outcome}` rows for completion and suppression (optional scrape inputs) | Ticket, not page: finding growth after a healthy or pending completion with no failed-pass growth in the window; mid-pass scrapes retain all baselines, and a failed completion consumes the finding silently so split scrapes never multiply its failed-pass ticket. First complete sample and any backwards counter re-baseline silently; missing or invalid rows are no window. The other error sites pair with a failed pass |
| `automod_sanction_refused` | `two_bot_automod_decisions_total{outcome="sanction_refused"}` (optional scrape input, so older containers without it hold the previous state) | Ticket, not page: any increase between valid samples (one ticket per window with new refusals); the other seven outcomes are policy working as designed; unknown outcomes collapse to `sanction_refused` in Rust, so a ticket can also mean a new outcome needs classification; baseline, restart and gap semantics mirror `feed_fetch_failures` |
| `moderation_enforcement_failures` | `two_bot_moderation_enforcements_total` summed over every verb and `discord_error`/`storage_error` (optional scrape input, so older containers without it hold the previous state) | Ticket, not page: any summed increase between valid samples (one ticket per window with new failures); `success` landed, `refused` is policy proving no mutation, and `other` is an unknown collapse that is not evidence of failure; baseline, restart and gap semantics mirror `feed_fetch_failures` |
| `voice_rest_stuck` | `two_bot_voice_rest_in_flight_age_seconds` (optional scrape input, so older containers without it hold the previous state) | Ticket, not page: age above 60 s (six times the 10 s wire timeout) in 3 consecutive valid samples; a valid sample at or below 60 s resolves, and missing/invalid samples break the streak but preserve a firing ticket |
| `voice_global_backoff_capped` | `two_bot_voice_global_backoff_capped_total` (optional scrape input, so older containers without it hold the previous state) | Ticket, not page: any increase between valid samples (one ticket per window with new caps); the shed is the designed backpressure shape under a past-30 s global wait; baseline, restart and gap semantics mirror `feed_fetch_failures` |

Deliberately unwatched: `two_bot_feed_fetches_total` (every non-success
attempt pairs 1:1 with an errors increment in `record_feed_fetch`, so the
errors sum carries the same signal with finer reason codes while `success`
is volume, not a fault), `two_bot_presence_capture_total` (no production
callsite records the family yet; the rule lands with the event-store S3
producer rather than watching zeros), `two_bot_voice_join_occupancy_total`
(distribution telemetry for capacity curves; no bucket is a fault), and
`two_bot_eventsub_reconcile_actions_total` (successful mutations are the
healthy signal; failures ticket through the failed-pass rule). Within
consumed families, `success`/`refused` moderation outcomes, non-refusal
automod outcomes, and the paired reconcile error sites stay quiet for the
reasons their rule rows name.

Deliberately unwatched: `two_bot_rest_guard_breaker_closes_total`. A close is
the 600 s invalid-request window cooling below threshold after a trip — pure
recovery, not a new fault. The opens ticket and the sustained-open ticket
already name the episode, and a close cannot occur without an observed open
(the guard toggles in-process; restarts re-baseline), so a closes rule would
ticket on every cooling with zero new information. Closes stay resolution
evidence: the open tickets clear plus the `discord_breaker_close` info log.

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

## Voice responder overload

| Event | Callsite (stable anchor) | Level | Fields | Meaning |
| --- | --- | --- | --- | --- |
| `voice responder saturated; interaction dropped without acknowledgement (sampled)` | `crates/bot/src/voice_rooms.rs` (`ResponderAdmission::record_drop`) | WARN | `interaction_id`: the refused interaction's snowflake as a string; `dropped`: cumulative refusals for this responder instance | All 16 responder permits are occupied. The refused interaction sends no callback and runs no command; no detached busy-response task is started. |

The counter increments on every refusal. The warning is sampled at most once
per 60-second wall-clock window per responder, shared across guilds and
interaction kinds; suppressed warnings do not extend the window. `dropped`
is a log field, not a metrics series. No interaction token, command arguments,
member name or response body is logged. This execution bound is distinct from
the shared router's lane, which continues to own `/kick`.

## Voice structured fields (gateway sink, adjacent)

`voice_event` fields in `crates/bot/src/voice_rooms.rs`; guild and channel
IDs are emitted as strings (notably `guild` on every `voice_join_received`
line and `channel` on every refused line), but no member, token, body, name
or presence content leaves the process. Included because the voice
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
| `voice_event="voice_admin_store"` | `crates/bot/src/voice_rooms.rs` (`log_admin_store_failure`: `voice admin store failed`) | One failed admin store read/write behind `/access`, `/logging`, `/export` or `/import`; bounded `op` (`access_read`, `access_save`, `logging_read`, `logging_save`, `export_read`, `import_read`) plus bounded `reason` (`unavailable`, `credential_refused`, `conflict`: the `StoreError` variant, no driver text) and the `guild_id`; the member reply is unchanged |

### `voice_event` JSON field shapes (verified 2026-10-17, emitter code only, no live log paste)

Field names and Rust types per `voice_event` value, verified against the
emitter in `crates/bot/src/voice_rooms.rs` (callsite = enclosing function,
never a line number). JSON keys match the field names; the message string is
the last argument to the log macro. In the JSON line these fields are
flattened at the root next to the envelope keys `ts`, `level`, `msg`,
`target`, `build_sha`, `release` and `spans` (`docs/logging.md`). None of
these lines sets `msg` and none of their messages is snake_case, so `msg` is
always `"tracing_event"` and the text above sits under `message`. Match on
`voice_event` or `message`, never `msg`.

| `voice_event` value | Fields (name: Rust type) | Emitter | Level / message |
| --- | --- | --- | --- |
| `voice_join` | `outcome`: `&'static str`; `queue_dwell_seconds`, `wait_before_dispatch_seconds`, `claim_seconds`, `reorder_seconds`, `post_seconds`, `persist_seconds`, `move_seconds`, `elapsed_seconds`: `f64` | `JoinTiming::finish` | info / `voice join completed` |
| `voice_operation` | `op`: `&'static str`; `outcome`: `&'static str` | `observe_voice_operation` | info / `voice_operation succeeded`; warn / `voice_operation failed` |
| `voice_respace` | `outcome`: `&'static str` | `observe_voice_respace` | info / `voice respace succeeded`; warn / `voice respace failed` |
| `voice_reconcile` | `delete_enqueued`, `suspended`, `resumed`, `succession_enqueued`: `u64` (emitted only when at least one is nonzero) | `reconcile` | info / `voice_reconcile planned` |
| `voice_dead_letter` | `action`: `&str` (bounded family); `attempts`: `u32` (queue budget constant) | `mark_failed_observed` | warn / `voice action dead-lettered` |
| `voice_creator_orphan` | `outcome`: `&str` (always `manual_needed`) | `execute_create` | warn / `voice creator orphan needs manual deletion` |
| `voice_join_received` accepted | `guild`: `String` | `accept_join_received` | info / `voice creator join accepted` |
| `voice_join_received` refused | `guild`, `channel`: `String`; `reason`: `&str` (bounded: `halted`, `access`, `unknown_creator`, `stale_ticket`, `duplicate`, `name_blocked`, unknown folds to `other` in the counter); `creators_len`: `u64`; `known`, `halted`, `ready`: `bool`; `live_generation`, `ticket_generation`: `u64` | `accept_join_received` | warn when `reason` is `halted`, debug when `duplicate`, info otherwise (including `name_blocked`) / `voice creator join refused before queueing` |
| `voice_admin_store` | `op`: `&'static str` (bounded: `access_read`, `access_save`, `logging_read`, `logging_save`, `export_read`, `import_read`); `reason`: `&'static str` (bounded: `unavailable`, `credential_refused`, `conflict`); `guild_id`: `String` | `log_admin_store_failure` | warn / `voice admin store failed` |

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

Adjacent actor messages without a `voice_event` field:
`voice actor load failed; retrying with capped backoff` and
`voice actor load failed; credential refused, not retrying`
(`observe_voice_actor_load_failure` in `crates/bot/src/voice_rooms.rs`):
the guild actor's store load failed. Every failure bumps
`two_bot_db_errors_total{op="other"}`; the warn is throttled process-wide to
one line per 5 minutes, so a burst across guilds names only the first.
Transient failures (including a load attempt abandoned at its 30 s
deadline) retry inside the same actor task: 1 s doubling to a 60 s cap,
until the load succeeds or the guild is removed. A credential refusal is
never retried; that actor respawns only on the next guild snapshot
(`publish_snapshot` on `GuildCreate`/resumed replay), and voice, channel
and role events for the guild are dropped until then.

`voice succession handed the room to the longest-present member`
(info; `apply_succession` in `crates/bot/src/voice_rooms.rs`): the
reconcile pass handed an ownerless-but-occupied room to its
longest-present member. Fields: `channel_id`, `previous_owner_id`,
`next_owner_id` (IDs only, no member names). One line per handoff; the
`voice_reconcile` aggregate counts it under `succession_enqueued`, and
the next tick finds the new owner present and stays silent. The refused
path keeps the existing `voice succession refused` warn.

`voice live view was not ready on a connected gateway; republished from cache`
(warn; `heal_not_ready` in `crates/bot/src/voice_rooms.rs`, guild-only field,
no IDs): READY or RESUMED was handled but a guild's live voice view was still
not ready, so creator joins in that guild were being dropped before a ticket.
The next dispatch republished the view from the gateway cache; bumps
`two_bot_voice_live_self_heal_total`. Rare by design: one line per repair,
never per VOICE_STATE_UPDATE.

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
The stuck, timeout and recovery counters feed the `voice_watchdog_stucks`,
`voice_dispatch_timeouts` and `voice_halt_recoveries` tickets (any increase
between valid samples); the re-probe skip/timeout lines stay log-only.

Empty-grace override warning (in `stored_empty_grace_with` in
`crates/bot/src/voice_rooms.rs`, guild-only fields, no IDs):
`stored empty-grace override out of range; using boot fallback` (warn): the
stored per-guild `TWO_TEMP_VOICE_EMPTY_GRACE_SECONDS` row is malformed or
outside 0 to 600 whole seconds, so the sweep keeps the boot fallback. The
line carries the setting key and the `out_of_range` reason.

Fatal boot-setting warning (in `invalid_boot_setting`, called by
`build_production_runtime` in `crates/bot/src/voice_rooms.rs`):
`temp-voice boot setting invalid; refusing voice runtime` (warn). Fields:
`key`: `&'static str` (one fixed key below); `reason`: `&'static str`
(always `invalid_value`). No configured values, parser errors, tokens,
channel/member lists or synthetic identity input are logged. This precedes
the unchanged `RoomHttpError::InvalidRequest` setup failure and the gateway's
`voice HTTP setup failed` line. It refuses the voice runtime, not necessarily
the process. Exactly one warning names the first invalid setting in existing
validation order:

1. Protected infrastructure channels: `DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID`,
   `TWO_TEMP_VOICE_GENERATOR_CHANNEL_ID`, `TWO_TEMP_VOICE_CATEGORY_ID`, then
   `TWO_TEMP_VOICE_PROTECTED_CHANNEL_IDS`. The exact refused sub-key is named.
2. Generator channel: `TWO_TEMP_VOICE_GENERATOR_CHANNEL_ID` (also checked by
   protection above, so malformed input normally refuses there first).
3. Empty-room grace: `TWO_TEMP_VOICE_EMPTY_GRACE_SECONDS` (whole seconds,
   0 to 600).
4. Synthetic humans: `TWO_TEMP_VOICE_SYNTHETIC_HUMAN_IDS` (up to four distinct
   nonzero IDs).

Absent, blank and valid settings remain silent. Nonfatal admission knobs
retain their separate default-fallback warning below. For safe triage, see
[voice boot-setting refusal](runbook-observability.md#voice-boot-setting-refusal).

Configured-channel notice fallback warning (in
`GuildRoomWorker::deliver_notice` in `crates/bot/src/voice_rooms.rs`, guild
field plus bounded `op` and `outcome`, no notice text or mention payload):
`voice notice configured channel failed; trying fallback` (warn): the
configured `/logging` channel refused the notice send, so delivery falls
through to the system channel, owner DM and creator chat in order. One line
per failed configured-channel attempt; a working channel logs nothing, and a
rate-limited attempt falls through silently (a rate limit is a retry, not an
outcome).

Invite-code override warning (in `stored_invite_code_with` in
`crates/bot/src/voice_rooms.rs`, guild-only fields, no IDs):
`stored invite-code override invalid; using vanity fallback` (warn): the
stored per-guild `TWO_GUILD_INVITE_CODE` row is not a JSON string of 2 to
32 ASCII alphanumerics plus `-`/`_`, so the `/invite` reply keeps the
gateway vanity fallback. The line carries the setting key and the
`invalid_invite_code` reason, never the rejected value.

Boot admission warning (in `configured_admission` in
`crates/bot/src/voice_rooms.rs`, key-only fields, no IDs):
`temp-voice admission setting invalid; using default` (warn): one of the
`TWO_TEMP_VOICE_MAX_PER_USER` (1 to 10), `TWO_TEMP_VOICE_MAX_PER_GUILD`
(1 to 45) or `TWO_TEMP_VOICE_CREATE_COOLDOWN_SECONDS` (0 to 3600) boot
values is unparsable or out of range, so that knob alone falls back to
its default (1, 40, 0 respectively) while the other knobs are still
honored. The line carries the setting key and the `invalid_value` reason.

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

## Verification notes (all three TBDs resolved 2026-10-17)

- Session-commit search re-run 2026-10-17: `grep -rn 'tracing::\(info\|warn\|error\)' crates/ --include='*.rs'` (pinned fully-qualified form) plus an offline extractor mirror over the six scoped sources that also covers bare `info!` / `warn!` / `error!` / `debug!` / `trace!` (same rule as the conformance suite: last string literal is the event name). Result: 84 unique messages, identical to the pinned catalog set (20 gateway + 9 main traced + 5 server + 2 shutdown + 1 jobs + 16 voice catalog + 14 main adjacent + 16 voice adjacent + 1 test-only). The two main drain strings are checked separately for presence and are not included in this total. Added: none. Removed: none. Sources: the six `SCOPED_SOURCES` files (`gateway.rs`, `main.rs`, `server.rs`, `shutdown.rs`, `jobs.rs`, `voice_rooms.rs`) plus the conformance pin in `crates/bot/src/observability_event_conformance_tests.rs`.
- `voice_event` JSON field shapes recorded 2026-10-17 from the emitters (names + Rust types; no live log paste): see the `voice_event` JSON field shapes table above. Verified against `JoinTiming::finish`, `observe_voice_operation`, `observe_voice_respace`, `reconcile`, `mark_failed_observed`, `execute_create` and `accept_join_received` in `crates/bot/src/voice_rooms.rs`; refusal vocabulary cross-checked against the bounded allowlist in `crates/core/src/metrics.rs` (`VOICE_JOIN_REFUSAL_REASONS`), where unknown reasons fold to `other`. JSON formatting ownership stays separate; only shapes are pinned here.
- Worker-side `container_gateway_failure` spelling cross-checked 2026-10-17: the keepalive tick in `wrangler/src/index.ts` (`keepalive`) emits `event: "container_gateway_failure"` with keys `event`, `phase`, `class`, `failed_step`, `error_code`, `build_revision`, `release`, `worker_version`, `log_url`. Spelling matches `docs/startup-diagnostics.md`, which records the abbreviated shape with placeholders for `phase`, `class`, `release` and `worker_version`; the four extra keys (`failed_step`, `error_code`, `build_revision`, `log_url`) are keepalive-tick context, not a spelling drift. Verified: event name identical, shared keys identical.

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
