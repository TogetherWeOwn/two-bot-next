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
| `eventsub reconcile parked: TWO_EVENTSUB is not exactly 1` | `crates/bot/src/eventsub_reconcile.rs` (`startup_reconcile`, `spawn_if_enabled`) | Info; gate dark at spawn or per pass, no subscription is listed or mutated (`status = "parked"`, `reason = "flag_dark"`) |
| `eventsub reconcile parked: the EventSub receiver is dark, no shared secret snapshot` | `crates/bot/src/eventsub_reconcile.rs` (`spawn_if_enabled`) | Info; no receiver snapshot at spawn, so no task and no pass (`status = "parked"`, `reason = "receiver_dark"`) |
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
| `two_bot_gateway_missed_events_total` | Sequence gaps inside one session; any nonzero increase fails the zero-missed-events acceptance |
| `two_bot_gateway_latency_seconds` | Last heartbeat ACK round-trip; `NaN` until measured |
| `two_bot_gateway_prefix_trigger_refused_total{reason="verdict"}` | In-scope prefix candidates refused by the automod verdict; the worker records candidates even when its capture-only gate prevents detached dispatch. Unmatched content, scope mismatches, bot/webhook messages and disabled text-command gates never increment it |
| `two_bot_presence_capture_total{outcome}` | Presence-update capture decisions on the hot path; `outcome` is `inserted` (first delivery stored a row), `duplicate` (same-key retry absorbed by the dedup key) or `rejected` (refused before the insert) |
| `two_bot_automod_decisions_total{outcome}` | Terminal automod inspection outcomes; `outcome` is `accepted`, `already_processed`, `dry_run`, `protected`, `deleted`, `warned`, `timed_out` or `sanction_refused`. Recorded once per committed `StoredOutcome` by the shared activation; replays and retained claims never count. Fixed eight series from process start; no message text, excerpts or user IDs become labels |

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
| `two_bot_voice_worker_halted` | None (label-free gauge) | Additional series: accepted but not required by the scrape validator, which pins only the alert-input label sets; read manually from the container exposition or the authenticated pull | Consumed by no alert rule; manual check named in the runbook fast-burn, slow-burn and creator-map-stale first responses | 1 while any current guild actor is halted by a store CredentialRefused or Discord 401, otherwise 0; a healthy guild cannot hide another guild's halt |
| `two_bot_voice_queue_pending` | Fixed `lane`: `urgent` or `deferred` (exactly two series) | Same additional-series handling as above | Consumed by no alert rule; manual queue-side complement to the join-budget burn rules | Queued (not in-flight) voice actions per lane, saturating-summed across all current guild actors |
| `two_bot_voice_queue_oldest_age_seconds` | None (label-free gauge) | Same additional-series handling as above | Consumed by no alert rule; manual queue-side complement to the join-budget burn rules | Age in seconds of the oldest queued voice action, the maximum across all current guild actors; 0 when every queue is empty |
| `two_bot_voice_live_not_ready_seconds` | None (label-free gauge) | Same additional-series handling as above | Consumed by no alert rule yet; the host watchdog reads it directly (alert when it stays above 120 s) | Seconds a guild's live voice view has been not ready while the gateway is connected (READY or RESUMED handled), the maximum across all current guild actors; 0 when every view is ready or the gateway is down. While above 0 that guild's creator joins are dropped without an accept or refusal line |
| `two_bot_voice_live_self_heal_total` | None (label-free counter) | Same additional-series handling as above | Consumed by no alert rule; any increase means a live view was left not ready on a connected gateway and was republished from the gateway cache | Live voice views republished by the not-ready-while-connected self-heal |

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

## Feed fetch series

Counters emitted by the feed poller render. Unknown `outcome` values collapse
to `failure` and unknown `error_class` values collapse to `other`, and no
source URL, host, header, body or feed id reaches any label.

| Series | Meaning |
| --- | --- |
| `two_bot_feed_fetches_total{outcome}` | Feed fetch attempts by bounded outcome; `outcome` is `success` (2xx fetch), `invalid_source` (source refused before dialling), `refused_redirect` (redirect refused by policy) or `failure` (every other failure, including non-2xx HTTP) |
| `two_bot_feed_fetch_errors_total{error_class}` | Failed feed fetches by bounded error class; `error_class` is `invalid_source`, `non_public_address`, `redirect_refused`, `too_large`, `invalid_encoding`, `unsupported_content_type`, `unsupported_encoding`, `resolve`, `transport`, `deadline`, `http_error` (completed non-2xx fetch) or `other`; successes record nothing here |

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
| `two_bot_moderation_enforcements_total{verb,outcome}` | Terminal moderation enforcements by bounded verb and outcome |

## Retention replay series

Label-free counter emitted by the retention purge-skip path in
`crates/bot/src/gateway.rs` (`retention_lifecycle`) and rendered by
`crates/core/src/metrics.rs`. No guild id, SQLSTATE or error text ever
becomes a label. Manual check, not an alert input: consumed by no alert
rule. Full semantics live in `docs/metrics.md`.

| Series | Meaning |
| --- | --- |
| `two_bot_retention_replay_needed_total` | Skipped retention purges needing Operator replay: one per real guild removal whose purge-queue write hit the schema gap; successful queue writes and rejoin-cancel skips never increment it |

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
